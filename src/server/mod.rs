use std::{collections::HashMap, net::TcpListener, sync::Arc, time::Duration};

use actix_web::{
    dev::Server, guard, http, middleware, web, App, HttpRequest, HttpResponse, HttpServer,
};
use log::{debug, error, info, warn};
use regex::Regex;
use serde::Deserialize;
use url::Url;

use crate::{
    configs::{conf, conf_u64, Conf, ConfName, DEFAULT_FFMPEG_TIMEOUT_SECS},
    feed_serving::{self, FeedServing, ServedFeed, ServingPolicy},
    provider::{self, MediaProvider},
    rss_transcodizer,
    transcoder::{FfmpegParameters, Transcoder},
};

mod headers;

pub fn spawn_server(listener: TcpListener) -> eyre::Result<Server> {
    spawn_server_with(listener, None)
}

/// Test wiring for the route-level transcode tests (issue #183): the real
/// server with one [`StreamUrlResolver`] installed behind the transcode
/// route — the existing resolution seam, driven with the provider fake and
/// the scripted origin over loopback. Production never installs one: every
/// request builds its own pipeline.
#[cfg(test)]
pub(crate) fn spawn_server_with_resolver(
    listener: TcpListener,
    resolver: Arc<dyn crate::transcoder::stream_url::StreamUrlResolver>,
) -> eyre::Result<Server> {
    spawn_server_with(listener, Some(resolver))
}

fn spawn_server_with(
    listener: TcpListener,
    transcode_resolver: Option<Arc<dyn crate::transcoder::stream_url::StreamUrlResolver>>,
) -> eyre::Result<Server> {
    let root = conf().get(ConfName::SubfolderPath).unwrap();
    Ok(HttpServer::new(move || {
        let app = App::new()
            .wrap(middleware::NormalizePath::new(
                middleware::TrailingSlash::MergeOnly,
            ))
            .service(
                web::scope(&root)
                    .service(
                        web::resource("transcode_media/to.mp3")
                            .name("transcode_mp3")
                            .guard(guard::Any(guard::Get()).or(guard::Head()))
                            .to(transcode_to_mp3),
                    )
                    .service(
                        //this is an old URL used in old vod2pod versions that did not work with
                        //itunes kept for backwards compatiility
                        web::resource("transcode_media/to_mp3")
                            .name("transcode_mp3_obsolete")
                            .guard(guard::Any(guard::Get()).or(guard::Head()))
                            .to(transcode_to_mp3),
                    )
                    .route("transcodize_rss", web::get().to(transcodize_rss))
                    .route("transcodize_rss", web::head().to(transcodize_rss))
                    .route("health", web::get().to(health))
                    .route("/", web::get().to(index))
                    .route("", web::get().to(index)),
            );
        match transcode_resolver.clone() {
            Some(resolver) => app.app_data(resolver),
            None => app,
        }
    })
    .listen(listener)?
    .run())
}

/// The transcoder the transcode route builds: production resolves behind the
/// real pipeline; the route-level tests drive the existing
/// [`StreamUrlResolver`] seam instead — the resolver a test installed as app
/// data (issue #183). Production installs none, so the lookup is always
/// `None` there and the behavior is exactly what it was.
async fn build_transcoder(
    req: &HttpRequest,
    parameters: &FfmpegParameters,
) -> eyre::Result<Transcoder> {
    match req.app_data::<Arc<dyn crate::transcoder::stream_url::StreamUrlResolver>>() {
        Some(resolver) => Transcoder::with_resolver(parameters, resolver.as_ref()).await,
        None => Transcoder::new(parameters).await,
    }
}

async fn health(query: web::Query<HashMap<String, String>>) -> HttpResponse {
    // Plain `/health` is the liveness probe: process-only, green while the
    // server runs. The `deep` variant is the readiness probe: it pings
    // Redis, the app's one hard dependency (ADR 0003), so a broken Redis
    // shows up as a not-ready pod instead of silently 503ing downloads.
    if query.get("deep").map(String::as_str) != Some("1") {
        return HttpResponse::Ok().finish();
    }
    match deep_redis_check().await {
        Ok(()) => HttpResponse::Ok().finish(),
        Err(e) => HttpResponse::ServiceUnavailable().body(format!("redis unreachable: {e}")),
    }
}

/// PING Redis on a fresh connection, bounded well under the k8s probe
/// timeout so a hung Redis yields a fast 503, never a hanging probe.
async fn deep_redis_check() -> eyre::Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut con = crate::get_redis_client().await?;
        let _: String = redis::cmd("PING").query_async(&mut con).await?;
        Ok::<(), eyre::Report>(())
    })
    .await
    .map_err(|_| eyre::eyre!("redis ping timed out"))?
}

async fn index(req: HttpRequest) -> HttpResponse {
    if let (Some(user_agent), Some(remote_addr), Some(referer)) = (
        req.headers().get("User-Agent"),
        req.connection_info().peer_addr(),
        req.headers().get("Referer"),
    ) {
        info!(
            "serving homepage - User-Agent: {}, Remote Address: {}, Referer: {}",
            user_agent.to_str().unwrap(),
            remote_addr,
            referer.to_str().unwrap()
        );
    }

    let html = std::fs::read_to_string("./templates/index.html").unwrap();

    HttpResponse::Ok().content_type("text/html").body(html)
}
/// The source-authorization guard shared by both routes (issue #183): a URL
/// is served only when the provider it dispatches to whitelists it — the one
/// SSRF defense every accepted URL passes. `Some` is the 403 to serve.
fn unauthorized_source_error(url: &Url, provider: &dyn MediaProvider) -> Option<HttpResponse> {
    if provider
        .domain_whitelist_regexes()
        .iter()
        .any(|r| r.is_match(url.as_ref()))
    {
        return None;
    }
    error!("supplied url ({url}) not in whitelist (whitelist is needed to prevent SSRF attack)");
    Some(HttpResponse::Forbidden().body("scheme and host not in whitelist"))
}

async fn transcodize_rss(
    req: HttpRequest,
    query: web::Query<HashMap<String, String>>,
) -> HttpResponse {
    if req.method() == http::Method::HEAD {
        return HttpResponse::Ok().finish();
    }

    let should_transcode = match conf().get(ConfName::TranscodingEnabled) {
        Ok(value) => !value.eq_ignore_ascii_case("false"),
        Err(_) => true,
    };

    if !should_transcode {
        warn!("transcoding is disabled");
    }
    let url = if let Some(x) = query.get("url") {
        x
    } else {
        error!("no url provided");
        return HttpResponse::BadRequest().finish();
    };

    let transcode_service_url = req.url_for("transcode_mp3", [""]).unwrap();

    let parsed_url = match Url::parse(url) {
        Ok(x) => x,
        Err(e) => return HttpResponse::BadRequest().body(e.to_string()),
    };

    let provider: Arc<dyn MediaProvider> = Arc::new(provider::from(&parsed_url));

    if let Some(forbidden) = unauthorized_source_error(&parsed_url, provider.as_ref()) {
        return forbidden;
    }

    let Ok(redis) = crate::get_redis_client().await else {
        error!("could not get redis client");
        return HttpResponse::InternalServerError().finish();
    };

    // the Feed Serving module owns the Degradation Ladder, stale-while-
    // revalidate included; the adapter reads the policy numbers from config
    // and injects them at construction (the module itself reads no
    // environment configuration)
    let cache: Arc<dyn feed_serving::FeedCacheStore> =
        Arc::new(feed_serving::RedisFeedCache::new(redis.clone()));
    let serving = FeedServing::new(
        serving_policy(),
        Arc::new(feed_serving::WallClock),
        Arc::new(feed_serving::ActixSpawner),
    );

    match serving.serve(cache, &parsed_url, provider).await {
        Ok(served) => {
            match served.freshness {
                feed_serving::Freshness::Fresh => {
                    info!("serving cached rss feed for {parsed_url}");
                }
                // the module scheduled the revalidation itself
                feed_serving::Freshness::Stale => {
                    info!("serving stale rss feed for {parsed_url}");
                }
                // the module logs the conversion itself
                feed_serving::Freshness::FreshlyGenerated => {}
            }
            serve_feed(&req, served, should_transcode, &transcode_service_url)
        }
        // the ladder is exhausted: no cached copy, the conversion failed and
        // the quota-free rung could not produce anything either
        Err(e) => {
            debug!("degradation ladder exhausted for {parsed_url}: {e}");
            HttpResponse::Conflict().finish()
        }
    }
}

/// Max-Age sent when serving a stale copy: tell the client to come back soon,
/// the background revalidation is likely done by then.
const STALE_SERVE_MAX_AGE_SECS: u64 = 60;

fn fresh_ttl_secs() -> u64 {
    conf_u64(ConfName::CacheTTL, crate::configs::DEFAULT_CACHE_TTL_SECS)
}

fn stale_max_age_secs() -> u64 {
    conf_u64(
        ConfName::StaleMaxAge,
        crate::configs::DEFAULT_STALE_MAX_AGE_SECS,
    )
}

fn max_fresh_period_secs() -> u64 {
    conf_u64(
        ConfName::MaxFreshPeriod,
        crate::configs::DEFAULT_MAX_FRESH_PERIOD_SECS,
    )
}

/// The serving policy numbers, read from config here (in the adapter) so the
/// Feed Serving module itself reads no environment configuration.
fn serving_policy() -> ServingPolicy {
    ServingPolicy {
        fresh_ttl_secs: fresh_ttl_secs(),
        stale_window_secs: stale_max_age_secs(),
        max_fresh_period_secs: max_fresh_period_secs(),
        stale_serve_max_age_secs: STALE_SERVE_MAX_AGE_SECS,
        degraded_fresh_ttl_secs: feed_serving::DEGRADED_FRESH_TTL_SECS,
        fresh_ttl_jitter_secs: feed_serving::FRESH_TTL_JITTER_SECS,
    }
}

/// Inject the per-request customizations into the served body and answer the
/// client, honoring conditional GET (If-None-Match / If-Modified-Since).
fn serve_feed(
    req: &HttpRequest,
    served: ServedFeed,
    should_transcode: bool,
    transcode_service_url: &Url,
) -> HttpResponse {
    match rss_transcodizer::inject_vod2pod_customizations(
        served.body,
        should_transcode.then(|| transcode_service_url.clone()),
    ) {
        Ok(body) => conditional_response(
            req.headers(),
            body,
            served.generated_at,
            served.max_age_secs,
        ),
        Err(e) => {
            error!("could not inject vod2pod customizations into feed");
            error!("{e}");
            HttpResponse::Conflict().finish()
        }
    }
}

/// Build the HTTP response for a feed body with ETag / Last-Modified /
/// Cache-Control and 304 handling. Pure function so tests can exercise it.
fn conditional_response(
    headers: &http::header::HeaderMap,
    body: String,
    generated_at: i64,
    max_age_secs: i64,
) -> HttpResponse {
    let etag = headers::etag_of(&body);
    let cache_control = format!("public, max-age={max_age_secs}");

    // RFC 7232: If-None-Match takes precedence over If-Modified-Since
    let client_has_current = headers
        .get(http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|if_none_match| headers::etag_matches(if_none_match, &etag))
        .unwrap_or_else(|| {
            headers
                .get(http::header::IF_MODIFIED_SINCE)
                .and_then(|value| value.to_str().ok())
                .map(|if_modified_since| {
                    headers::http_date_at_or_after(if_modified_since, generated_at)
                })
                .unwrap_or(false)
        });

    if client_has_current {
        debug!("client already holds the current feed, serving 304");
        return HttpResponse::NotModified()
            .insert_header((http::header::ETAG, format!("\"{etag}\"")))
            .insert_header((http::header::CACHE_CONTROL, cache_control))
            .finish();
    }

    HttpResponse::Ok()
        .content_type("application/rss+xml")
        .insert_header((http::header::ETAG, format!("\"{etag}\"")))
        .insert_header((http::header::CACHE_CONTROL, cache_control))
        .insert_header((
            http::header::LAST_MODIFIED,
            headers::http_date(generated_at),
        ))
        .body(body)
}

#[derive(Deserialize)]
struct TranscodizeQuery {
    url: Url,
    bitrate: usize,
    duration: usize,
}

/// Parse a Range header into the requested byte window (inclusive on both
/// ends), defaulting the end to the last advertised byte. Whether the window
/// can be served at all is the decision core's call — the caller checks it
/// before deriving any byte counts from the pair, so a reversed or
/// past-the-end window never reaches arithmetic that would underflow on it
/// (surfaced by the decision-core pins, issue #183).
fn parse_range_header(
    content_range_str: &str,
    advertised_bytes: usize,
) -> eyre::Result<(usize, usize)> {
    let re = Regex::new(r"(?P<start>[0-9]{1,20})-?(?P<end>[0-9]{1,20})?")?;
    let captures = if let Some(x) = re.captures_iter(content_range_str).next() {
        x
    } else {
        return Err(eyre::eyre!("content range regex failed"));
    };

    let mut start = 0;
    if let Some(x) = captures.name("start") {
        start = x.as_str().parse()?;
    }

    if advertised_bytes == 0 {
        error!("The requested Rage header with a length of 0 is invalid: {content_range_str}");
        return Err(eyre::eyre!(
            "The requested Rage header with a length of 0 is invalid: {content_range_str}"
        ));
    }
    let mut end = advertised_bytes - 1;
    if let Some(x) = captures.name("end") {
        end = x.as_str().parse()?;
    }

    if end == start {
        return Err(eyre::eyre!(
            "The requested Rage header with a length of 0 is invalid: {content_range_str}"
        ));
    }

    Ok((start, end))
}

/// The transcode route's decision core: from the raw request facts — the
/// Range header (if any), the advertised byte count and the declared
/// duration — decide the byte window to serve, the seek it maps to in the
/// source, and the status. Pure, like `conditional_response` beside it: the
/// route maps the decision onto HTTP and hands the window to the transcoder
/// (issue #183).
#[derive(Debug, PartialEq)]
struct TranscodeWindow {
    /// The advertised length both answers echo in Content-Range.
    total_streamable_bytes: usize,
    start_bytes: usize,
    end_bytes: usize,
    /// The count the transcode stream must deliver — the window's own
    /// length; the stdout pump pads or truncates to exactly this.
    expected_bytes: usize,
    /// Where the window starts in the source, in seconds.
    seek_secs: f32,
    /// True when the window starts mid-stream: the answer is 206, not 200.
    /// A seek within a tenth of a second of the start still counts as "from
    /// the start" — podcatchers send `bytes=0-` with rounding noise.
    partial: bool,
}

/// Why the route cannot serve the window: a malformed Range header maps to
/// 400, a window past the advertised length to 416.
#[derive(Debug, PartialEq)]
enum TranscodeWindowError {
    MalformedRange(String),
    RangeNotSatisfiable,
}

fn decide_transcode_window(
    range_header: Option<&http::header::HeaderValue>,
    total_streamable_bytes: usize,
    duration_secs: usize,
) -> Result<TranscodeWindow, TranscodeWindowError> {
    const DEFAULT_CONTENT_RANGE: &str = "0-";
    let content_range_str = match range_header {
        Some(x) => x.to_str().unwrap_or_default(),
        None => DEFAULT_CONTENT_RANGE,
    };

    debug!("received content range {content_range_str}");

    let (start_bytes, end_bytes) = parse_range_header(content_range_str, total_streamable_bytes)
        .map_err(|e| TranscodeWindowError::MalformedRange(e.to_string()))?;

    debug!("requested content-range: bytes {start_bytes}-{end_bytes}/{total_streamable_bytes}");

    // Before any byte arithmetic: a window past the advertised length is
    // not satisfiable. Checking here keeps the window count below free of
    // the underflow a reversed or past-the-end start used to produce
    // (surfaced by the decision-core pins, issue #183).
    if start_bytes > end_bytes || start_bytes > total_streamable_bytes {
        return Err(TranscodeWindowError::RangeNotSatisfiable);
    }

    let expected_bytes = end_bytes - start_bytes + 1;

    let seek_secs =
        ((start_bytes as f32) / (total_streamable_bytes as f32)) * (duration_secs as f32);
    debug!("choosen seek_time: {seek_secs}");

    Ok(TranscodeWindow {
        total_streamable_bytes,
        start_bytes,
        end_bytes,
        expected_bytes,
        seek_secs,
        partial: seek_secs > 0.1,
    })
}

/// The window headers both answers carry — the HEAD probe and the streamed
/// GET alike (issue #183).
fn insert_window_headers(
    mut builder: actix_web::HttpResponseBuilder,
    window: &TranscodeWindow,
) -> actix_web::HttpResponseBuilder {
    builder.insert_header(("Accept-Ranges", "bytes"));
    builder.insert_header((
        "Content-Range",
        format!(
            "bytes {}-{}/{}",
            window.start_bytes, window.end_bytes, window.total_streamable_bytes
        ),
    ));
    builder
}

async fn transcode_to_mp3(req: HttpRequest, query: web::Query<TranscodizeQuery>) -> HttpResponse {
    let stream_url = &query.url;
    let bitrate = query.bitrate;
    let duration_secs = query.duration;
    // same arithmetic the enclosure `length` attribute uses (see
    // rss_transcodizer::streamable_bytes), so advertised size == served size
    let total_streamable_bytes = usize::try_from(rss_transcodizer::streamable_bytes(
        duration_secs as u64,
        bitrate as u64,
    ))
    .unwrap_or(usize::MAX);
    info!("processing transcode at {bitrate}k for {stream_url}");

    if let Ok(value) = conf().get(ConfName::TranscodingEnabled) {
        if value.eq_ignore_ascii_case("false") {
            return HttpResponse::Forbidden().finish();
        }
    }

    let provider = provider::from(stream_url);

    if let Some(forbidden) = unauthorized_source_error(stream_url, &provider) {
        return forbidden;
    }

    // The decisions, purely; what is left here is only shaping.
    let window = match decide_transcode_window(
        req.headers().get("Range"),
        total_streamable_bytes,
        duration_secs,
    ) {
        Ok(window) => window,
        Err(TranscodeWindowError::MalformedRange(e)) => return HttpResponse::BadRequest().body(e),
        Err(TranscodeWindowError::RangeNotSatisfiable) => {
            return HttpResponse::RangeNotSatisfiable().finish()
        }
    };

    let timeout_in_seconds = conf()
        .get(ConfName::FfmpegTimeoutSeconds)
        .unwrap()
        .parse()
        .unwrap();
    debug!("choosen timeout in seconds: {timeout_in_seconds}");

    let codec = conf().get(ConfName::AudioCodec).unwrap().into();
    let ffmpeg_paramenters = FfmpegParameters {
        seek_time: window.seek_secs,
        url: stream_url.clone(),
        audio_codec: codec,
        bitrate_kbit: bitrate,
        max_rate_kbit: bitrate * 30,
        expected_bytes_count: window.expected_bytes,
        timeout_in_seconds,
    };
    debug!("seconds: {duration_secs}, bitrate: {bitrate}");

    if req.method() == http::Method::HEAD {
        // The HEAD probe derives its answer from the window alone — no
        // resolution, no staging, no transcoder (issue #183).
        return insert_window_headers(HttpResponse::Ok(), &window)
            .content_type(codec.get_mime_type_str())
            .finish();
    }

    match tokio::time::timeout(
        Duration::from_secs(
            timeout_in_seconds
                .try_into()
                .unwrap_or(DEFAULT_FFMPEG_TIMEOUT_SECS),
        ),
        build_transcoder(&req, &ffmpeg_paramenters),
    )
    .await
    {
        Ok(Ok(transcoder)) => {
            let stream = transcoder.get_transcode_stream();

            let response_builder = if window.partial {
                HttpResponse::PartialContent()
            } else {
                HttpResponse::Ok()
            };

            insert_window_headers(response_builder, &window)
                .content_type(codec.get_mime_type_str())
                .no_chunking((window.expected_bytes).try_into().unwrap())
                .streaming(stream)
        }
        Ok(Err(e)) => {
            // The 503 body alone is invisible to the operator: without this
            // line a failed resolution (e.g. Redis down — see ADR 0003)
            // leaves nothing in the logs but the request's first line, and
            // only the podcatcher's retries hint at the failure.
            error!("transcoder init failed for {stream_url}: {e:#}");
            HttpResponse::ServiceUnavailable().body(e.to_string())
        }
        Err(_) => {
            error!("transcoder init timed out for {stream_url} after {timeout_in_seconds}s");
            HttpResponse::ServiceUnavailable().body("transcoder initialization timed out")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_start_and_end_start_to_end() {
        let content_range_str = "bytes=0-99";
        let advertised_bytes = 100;
        let (start, end) = parse_range_header(content_range_str, advertised_bytes).unwrap();
        assert_eq!((start, end), (0, 99));
    }

    #[test]
    fn test_get_start_and_end_middle1_to_middle2() {
        let content_range_str = "bytes=50-199";
        let advertised_bytes = 200;
        let (start, end) = parse_range_header(content_range_str, advertised_bytes).unwrap();
        assert_eq!((start, end), (50, 199));
    }

    #[test]
    fn test_get_start_and_end_middle_to_undefined() {
        let content_range_str = "bytes=100-";
        let advertised_bytes = 200;
        let (start, end) = parse_range_header(content_range_str, advertised_bytes).unwrap();
        assert_eq!((start, end), (100, 199));
    }

    #[test]
    fn test_get_start_and_end_start_to_undefined() {
        let content_range_str = "bytes=0-";
        let advertised_bytes = 200;
        let (start, end) = parse_range_header(content_range_str, advertised_bytes).unwrap();
        assert_eq!((start, end), (0, 199));
    }

    #[test]
    fn test_get_start_and_end_past_the_advertised_length_does_not_underflow() {
        // The parse is window-shape only: a start past the advertised length
        // is the 416 case, decided by the caller before any count is derived
        // — it must not underflow the arithmetic on the way there.
        let (start, end) = parse_range_header("bytes=9000-", 8000).unwrap();
        assert_eq!((start, end), (9000, 7999));
    }

    #[test]
    fn test_conditional_response_declares_rss_content_type() {
        let response = conditional_response(
            &http::header::HeaderMap::new(),
            "<rss>feed body</rss>".to_string(),
            1_758_424_055,
            3600,
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            // the mime type podcatchers match feeds against
            Some("application/rss+xml")
        );
    }

    #[test]
    fn test_conditional_response_serves_304_on_if_none_match() {
        let body = "<rss>feed body</rss>";
        let etag = headers::etag_of(body);

        let mut req_headers = http::header::HeaderMap::new();
        req_headers.insert(
            http::header::IF_NONE_MATCH,
            http::header::HeaderValue::from_str(&format!("\"{etag}\"")).unwrap(),
        );

        let response = conditional_response(&req_headers, body.to_string(), 1_758_424_055, 3600);

        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);
        assert_eq!(
            response
                .headers()
                .get(http::header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some(format!("\"{etag}\"").as_str())
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("public, max-age=3600")
        );
    }

    #[test]
    fn test_conditional_response_serves_200_without_conditional_headers() {
        let body = "<rss>feed body</rss>";

        let response = conditional_response(
            &http::header::HeaderMap::new(),
            body.to_string(),
            1_758_424_055,
            3600,
        );

        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some(format!("\"{}\"", headers::etag_of(body)).as_str())
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok()),
            Some(headers::http_date(1_758_424_055).as_str())
        );
    }

    #[test]
    fn test_conditional_response_304_on_if_modified_since() {
        let generated_at = 1_758_424_055;

        let mut req_headers = http::header::HeaderMap::new();
        req_headers.insert(
            http::header::IF_MODIFIED_SINCE,
            http::header::HeaderValue::from_str(&headers::http_date(generated_at)).unwrap(),
        );

        let response = conditional_response(
            &req_headers,
            "<rss>feed body</rss>".to_string(),
            generated_at,
            3600,
        );

        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn test_conditional_response_if_none_match_takes_precedence() {
        // If-None-Match that does not match must win over an If-Modified-Since
        // that would produce a 304 (RFC 7232 section 6)
        let generated_at = 1_758_424_055;

        let mut headers = http::header::HeaderMap::new();
        headers.insert(
            http::header::IF_NONE_MATCH,
            http::header::HeaderValue::from_str("\"stale-etag\"").unwrap(),
        );
        headers.insert(
            http::header::IF_MODIFIED_SINCE,
            http::header::HeaderValue::from_str(&headers::http_date(generated_at)).unwrap(),
        );

        let response = conditional_response(
            &headers,
            "<rss>feed body</rss>".to_string(),
            generated_at,
            3600,
        );

        assert_eq!(response.status(), http::StatusCode::OK);
    }

    // The env pin must span the whole call (EnvConf reads env at call time),
    // so the std guard is held across the await. Safe: the lock's contenders
    // are other tests' threads — blocking serializes them; nothing on this
    // test's own runtime ever locks it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn deep_redis_check_fails_when_redis_unreachable() {
        let _guard = crate::test_support::env_lock();
        // Port 1 hosts no Redis in any environment, so the check must fail
        // fast with a connect error (refused in CI, denied in sandboxes) —
        // never hang and never report Ok.
        let _pin = (
            crate::test_support::EnvPin::set("REDIS_ADDRESS", "127.0.0.1"),
            crate::test_support::EnvPin::set("REDIS_PORT", "1"),
        );

        let verdict = deep_redis_check().await;

        assert!(
            verdict.is_err(),
            "deep check must fail with Redis unreachable"
        );
    }

    // ---- route-level transcode tests (issue #183) ----
    //
    // The loopback pattern of the staging test origin, pointed at the server
    // route for the first time: the real transcode route served on
    // 127.0.0.1:0, with the existing StreamUrlResolver seam wired to the
    // provider fake and the scripted origin. The suite pins the route's
    // current hand-off — request assembly, 200 vs 206 selection, timeout
    // wrapping, 503 mapping, HEAD without staging — before the hand-off
    // rework (issue #184) reshapes it. No live network, no yt-dlp, no Redis.
    //
    // Every test holds the env lock for its whole body: the handlers read
    // configuration (process env) per request, and one test's pins must not
    // leak into another test's server. Holding a std lock across an await is
    // safe here for the same reason the deep-Redis test below spells out:
    // the lock's contenders are other tests' threads, and nothing on the
    // test's own runtime ever locks it.

    use crate::rss_transcodizer::streamable_bytes;
    use crate::test_support::{
        env_lock, staging_test_limits, EnvPin, MediaFixture, OriginProvider, TempDir,
    };
    use crate::transcoder::staging::test_origin::{OriginScript, ScriptedOrigin};
    use crate::transcoder::staging::{stage_gate_of, StageLimits, StagingCheckStage};
    use crate::transcoder::stream_url::{
        ProviderStreamUrlResolver, ResolvedSource, StreamUrlResolver, TcpPreflight,
    };
    use async_trait::async_trait;

    /// The episode the requests describe: 2 s of audio at 32 kbit. The
    /// advertised length the route must echo is 2 × 32 × 1000 / 8 = 8000
    /// bytes — asserted as a literal below, so a changed byte-count formula
    /// (shared with the enclosure `length`, ADR 0002) cannot sneak past the
    /// pin.
    const ROUTE_DURATION_SECS: usize = 2;
    const ROUTE_BITRATE_KBIT: usize = 32;
    /// The advertised length those parameters work out to — the test's own
    /// arithmetic, not the code's.
    const ROUTE_TOTAL_BYTES: usize = 8000;

    /// The media URL the RSS enclosure points at — a YouTube watch URL, as
    /// production would carry. The route's whitelist guard dispatches the
    /// real YouTube provider for it; resolution itself belongs to the
    /// injected seam.
    fn media_url() -> Url {
        Url::parse("https://www.youtube.com/watch?v=UMO52N2vpk0").unwrap()
    }

    /// A resolution that fails — the 503 mapping's trigger.
    struct FailingResolver;

    #[async_trait]
    impl StreamUrlResolver for FailingResolver {
        async fn resolve(&self, _media_url: &Url) -> eyre::Result<ResolvedSource> {
            Err(eyre::eyre!("resolution failed: the provider lied"))
        }
    }

    /// A resolution that never answers — the timeout wrapper's trigger.
    struct HangingResolver;

    #[async_trait]
    impl StreamUrlResolver for HangingResolver {
        async fn resolve(&self, _media_url: &Url) -> eyre::Result<ResolvedSource> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            unreachable!("the route's timeout fires long before this")
        }
    }

    // The route reads configuration (process env) per request; the pins
    // below restore the ambient values on drop. `TRANSCODE` defaults to
    // "False" — transcoding disabled — and the route refuses with a bare
    // 403 before anything else, so every route test pins it on; the suite
    // exercises the route's real behavior, not the off-switch.

    /// The staged resolver wired like production, behind the real route: the
    /// direct-media provider fake resolved to the scripted origin, the real
    /// staging stage with tight test limits in an isolated temp dir, a gate
    /// of the test's own (issues #177, #181 — the resolver hands back the
    /// staged source already holding its slot). The `TempDir` must outlive
    /// the requests, so it is returned to the caller.
    fn staged_route_resolver(origin: &ScriptedOrigin) -> (Arc<dyn StreamUrlResolver>, TempDir) {
        let temp_dir = TempDir::new("route-staging");
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(OriginProvider(origin.url())),
            Arc::new(TcpPreflight::with_timeout(Duration::from_secs(2))),
            Some(Arc::new(StagingCheckStage::new(
                reqwest::Client::new(),
                StageLimits {
                    chunk_size: 1024 * 1024,
                    concurrency: 3,
                    attempt_timeout: Duration::from_secs(10),
                    stage_timeout: Duration::from_secs(120),
                    ..staging_test_limits()
                },
                temp_dir.path().to_path_buf(),
            ))),
            true,
            Some(stage_gate_of(4)),
        );
        (Arc::new(resolver), temp_dir)
    }

    /// Spawn the real server with `resolver` behind the transcode route on a
    /// loopback port, the way `tests/`' `spawn_app` does.
    fn spawn_route_server(resolver: Arc<dyn StreamUrlResolver>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(spawn_server_with_resolver(listener, resolver).unwrap());
        format!("http://127.0.0.1:{port}")
    }

    /// One transcode-route request: the shared query parameters, an optional
    /// Range header, the requested method.
    async fn transcode_request(
        address: &str,
        method: reqwest::Method,
        range: Option<&str>,
    ) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .request(method, format!("{address}/transcode_media/to.mp3"))
            .query(&[
                ("url", media_url().as_str().to_owned()),
                ("bitrate", ROUTE_BITRATE_KBIT.to_string()),
                ("duration", ROUTE_DURATION_SECS.to_string()),
            ]);
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        request.send().await.expect("the route must answer")
    }

    fn header<'a>(response: &'a reqwest::Response, name: &str) -> Option<&'a str> {
        response.headers().get(name).and_then(|v| v.to_str().ok())
    }

    #[allow(clippy::await_holding_lock)]
    #[actix_rt::test]
    async fn transcode_route_serves_a_staged_source_end_to_end() {
        let _env = env_lock();
        let _enabled = EnvPin::set("TRANSCODE", "true");
        let fixture = MediaFixture::new("route-e2e", ROUTE_DURATION_SECS as u32);
        let origin = ScriptedOrigin::start(
            0,
            OriginScript {
                content_override: Some(fixture.bytes.clone()),
                ..OriginScript::default()
            },
        );
        let (resolver, _stage_dir) = staged_route_resolver(&origin);
        let address = spawn_route_server(resolver);

        let response = transcode_request(&address, reqwest::Method::GET, None).await;

        assert_eq!(response.status(), 200);
        assert_eq!(header(&response, "accept-ranges"), Some("bytes"));
        assert_eq!(
            header(&response, "content-range"),
            Some("bytes 0-7999/8000"),
            "the window must cover the whole advertised length"
        );
        assert_eq!(
            response.content_length(),
            Some(ROUTE_TOTAL_BYTES as u64),
            "the served length must be the advertised one"
        );
        assert_eq!(header(&response, "content-type"), Some("audio/mpeg"));
        let body = response.bytes().await.unwrap();
        assert_eq!(
            body.len(),
            ROUTE_TOTAL_BYTES,
            "the transfer must deliver exactly the advertised bytes"
        );
        // The staged path through the post-seam resolver: ranged fetches
        // only — no open-ended GET reached the origin, and ffmpeg read the
        // staged file, not the network.
        assert_eq!(
            origin.requests_at(None),
            0,
            "no open-ended GET on the staged path"
        );
        assert!(
            origin.total_requests() > 0,
            "the source must have been staged"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[actix_rt::test]
    async fn transcode_route_answers_206_for_a_mid_stream_window() {
        let _env = env_lock();
        let _enabled = EnvPin::set("TRANSCODE", "true");
        let fixture = MediaFixture::new("route-seek", ROUTE_DURATION_SECS as u32);
        let origin = ScriptedOrigin::start(
            0,
            OriginScript {
                content_override: Some(fixture.bytes.clone()),
                ..OriginScript::default()
            },
        );
        let (resolver, _stage_dir) = staged_route_resolver(&origin);
        let address = spawn_route_server(resolver);

        // A podcatcher resuming mid-file: the window starts at the halfway
        // byte, so the seek is 1 s in and the answer is 206, not 200.
        let response = transcode_request(&address, reqwest::Method::GET, Some("bytes=4000-")).await;

        assert_eq!(response.status(), 206);
        assert_eq!(
            header(&response, "content-range"),
            Some("bytes 4000-7999/8000"),
        );
        let body = response.bytes().await.unwrap();
        assert_eq!(
            body.len(),
            ROUTE_TOTAL_BYTES - 4000,
            "the window's own length, not the whole stream"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[actix_rt::test]
    async fn transcode_route_maps_a_failed_resolution_to_503() {
        let _env = env_lock();
        let _enabled = EnvPin::set("TRANSCODE", "true");
        let address = spawn_route_server(Arc::new(FailingResolver));

        let response = transcode_request(&address, reqwest::Method::GET, None).await;

        assert_eq!(response.status(), 503);
        let body = response.text().await.unwrap();
        assert!(
            body.contains("resolution failed"),
            "the 503 body carries the resolution error: {body}"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[actix_rt::test]
    async fn transcode_route_times_out_slow_resolution_to_503() {
        let _env = env_lock();
        let _enabled = EnvPin::set("TRANSCODE", "true");
        // The wrapper's budget, pinned to 1 s: the hanging resolution must
        // be cut off by it, not by the resolver's own 30 s sleep.
        let _pin = EnvPin::set("FFMPEG_TIMEOUT_SECONDS", "1");
        let address = spawn_route_server(Arc::new(HangingResolver));

        let started = std::time::Instant::now();
        let response = transcode_request(&address, reqwest::Method::GET, None).await;

        assert_eq!(response.status(), 503);
        assert_eq!(
            response.text().await.unwrap(),
            "transcoder initialization timed out"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the route's timeout, not the resolver's sleep, must end the \
             request: took {:?}",
            started.elapsed()
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[actix_rt::test]
    async fn transcode_route_derives_head_response_without_staging() {
        let _env = env_lock();
        let _enabled = EnvPin::set("TRANSCODE", "true");
        let fixture = MediaFixture::new("route-head", ROUTE_DURATION_SECS as u32);
        let origin = ScriptedOrigin::start(
            0,
            OriginScript {
                content_override: Some(fixture.bytes.clone()),
                ..OriginScript::default()
            },
        );
        // The staged resolver stays wired: a HEAD that staged — or resolved
        // at all — would leave requests on the origin. It must leave none.
        let (resolver, _stage_dir) = staged_route_resolver(&origin);
        let address = spawn_route_server(resolver);

        let response = transcode_request(&address, reqwest::Method::HEAD, None).await;

        assert_eq!(response.status(), 200);
        assert_eq!(header(&response, "accept-ranges"), Some("bytes"));
        assert_eq!(
            header(&response, "content-range"),
            Some("bytes 0-7999/8000"),
        );
        assert_eq!(header(&response, "content-type"), Some("audio/mpeg"));
        assert_eq!(response.bytes().await.unwrap().len(), 0);
        assert_eq!(
            origin.total_requests(),
            0,
            "HEAD derives its response without staging — the origin must see nothing"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[actix_rt::test]
    async fn transcode_route_rejects_a_source_outside_the_whitelist() {
        // Production wiring: no resolver installed, the guard runs before
        // any resolution would even be built.
        let _env = env_lock();
        let _enabled = EnvPin::set("TRANSCODE", "true");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(spawn_server(listener).unwrap());
        let address = format!("http://127.0.0.1:{port}");

        let response = reqwest::Client::new()
            .get(format!("{address}/transcode_media/to.mp3"))
            .query(&[
                ("url", "https://evil.example.com/payload"),
                ("bitrate", "32"),
                ("duration", "2"),
            ])
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 403);
        assert_eq!(
            response.text().await.unwrap(),
            "scheme and host not in whitelist"
        );
    }

    /// The byte-count formula the route shares with the enclosure `length`
    /// attribute — pinned here so the route tests' literals above stay
    /// honest about what they describe.
    #[test]
    fn route_fixture_arithmetic_matches_the_shared_formula() {
        assert_eq!(
            streamable_bytes(ROUTE_DURATION_SECS as u64, ROUTE_BITRATE_KBIT as u64),
            ROUTE_TOTAL_BYTES as u64
        );
    }

    // ---- the decision core's unit pins (issue #183) ----
    //
    // The route's decisions — window, seek, status — as a pure function,
    // mirroring `conditional_response`. Each pin is a verdict a podcatcher
    // would observe: which bytes, seeking where, 200 or 206 or an error.

    fn range(value: &str) -> Option<&'static http::header::HeaderValue> {
        // Leaked on purpose: the decision core borrows the header, and the
        // pins have no owner to hold it.
        Some(Box::leak(Box::new(
            http::header::HeaderValue::from_str(value).unwrap(),
        )))
    }

    #[test]
    fn no_range_covers_the_whole_stream_from_the_start() {
        let window = decide_transcode_window(None, ROUTE_TOTAL_BYTES, ROUTE_DURATION_SECS).unwrap();
        assert_eq!(window.start_bytes, 0);
        assert_eq!(window.end_bytes, ROUTE_TOTAL_BYTES - 1);
        assert_eq!(window.expected_bytes, ROUTE_TOTAL_BYTES);
        assert_eq!(window.seek_secs, 0.0);
        assert!(!window.partial, "a whole-stream window is a 200");
    }

    #[test]
    fn an_open_ended_mid_stream_window_seeks_and_serves_206() {
        let window =
            decide_transcode_window(range("bytes=4000-"), ROUTE_TOTAL_BYTES, ROUTE_DURATION_SECS)
                .unwrap();
        assert_eq!(window.start_bytes, 4000);
        assert_eq!(window.end_bytes, ROUTE_TOTAL_BYTES - 1);
        assert_eq!(window.expected_bytes, ROUTE_TOTAL_BYTES - 4000);
        assert_eq!(
            window.seek_secs, 1.0,
            "4000 of 8000 bytes into a 2 s stream"
        );
        assert!(window.partial, "a mid-stream window is a 206");
    }

    #[test]
    fn a_bounded_window_keeps_the_requested_end_and_serves_200_while_the_seek_is_tiny() {
        // `bytes=50-199` of an 8000-byte stream: the seek is 50/8000 × 2 s =
        // 0.0125 s — inside the "from the start" threshold, so 200.
        let window = decide_transcode_window(
            range("bytes=50-199"),
            ROUTE_TOTAL_BYTES,
            ROUTE_DURATION_SECS,
        )
        .unwrap();
        assert_eq!(
            (window.start_bytes, window.end_bytes, window.expected_bytes),
            (50, 199, 150)
        );
        assert!(!window.partial);
    }

    #[test]
    fn a_seek_of_exactly_a_tenth_of_a_second_is_still_from_the_start() {
        // 400 of 8000 bytes into a 2 s stream is exactly the 0.1 s
        // threshold — `<=` keeps it a 200. Podcatchers send `bytes=0-` with
        // rounding noise; the threshold absorbs it.
        let window =
            decide_transcode_window(range("bytes=400-"), ROUTE_TOTAL_BYTES, ROUTE_DURATION_SECS)
                .unwrap();
        assert_eq!(window.seek_secs, 0.1);
        assert!(!window.partial);
    }

    #[test]
    fn a_window_past_the_advertised_length_is_not_satisfiable() {
        assert_eq!(
            decide_transcode_window(range("bytes=9000-"), ROUTE_TOTAL_BYTES, ROUTE_DURATION_SECS),
            Err(TranscodeWindowError::RangeNotSatisfiable)
        );
    }

    #[test]
    fn a_malformed_range_is_a_bad_request() {
        assert!(matches!(
            decide_transcode_window(range("bytes=abc"), ROUTE_TOTAL_BYTES, ROUTE_DURATION_SECS),
            Err(TranscodeWindowError::MalformedRange(_))
        ));
    }

    #[test]
    fn a_zero_advertised_length_cannot_serve_any_window() {
        // duration 0 advertises 0 bytes — the enclosure arithmetic rejects
        // the zero-length window before any byte math can divide by it.
        assert!(matches!(
            decide_transcode_window(range("bytes=0-"), 0, 0),
            Err(TranscodeWindowError::MalformedRange(_))
        ));
    }

    // ---- the source-authorization guard's pins (issue #183) ----
    // One guard, both routes: a URL is served only when the provider it
    // dispatches to whitelists it — the one SSRF defense every accepted URL
    // passes.

    #[test]
    fn the_guard_accepts_a_url_the_dispatched_provider_whitelists() {
        let url = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let provider = provider::from(&url);

        assert!(
            unauthorized_source_error(&url, &provider).is_none(),
            "a URL the dispatched provider whitelists must pass the guard"
        );
    }

    #[test]
    fn the_guard_rejects_a_foreign_url_with_a_403() {
        let _env = env_lock();
        // The fallback provider's whitelist is user-configurable; pin it to
        // the deployment default (empty) so the verdict is deterministic.
        let _pin = EnvPin::set("VALID_URL_DOMAINS", "");
        let url = Url::parse("https://evil.example.com/payload").unwrap();
        let provider = provider::from(&url);

        let error = unauthorized_source_error(&url, &provider)
            .expect("a URL no provider whitelists must be rejected");

        assert_eq!(error.status(), http::StatusCode::FORBIDDEN);
    }
}
