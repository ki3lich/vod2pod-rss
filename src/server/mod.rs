use std::{collections::HashMap, net::TcpListener, sync::Arc, time::Duration};

use actix_web::{
    dev::Server, guard, http, middleware, web, App, HttpRequest, HttpResponse, HttpServer,
};
use log::{debug, error, info, warn};
use serde::Deserialize;
use url::Url;

use crate::{
    configs::{conf, conf_u64, Conf, ConfName},
    feed_serving::{self, FeedServing, ServedFeed, ServingPolicy},
    provider::{self, MediaProvider},
    rss_transcodizer,
    transcoder::{
        request::{TranscodeRequest, TranscodeRequestError, TranscodeWindow},
        Transcoder,
    },
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
    request: &TranscodeRequest,
) -> eyre::Result<Transcoder> {
    match req.app_data::<Arc<dyn crate::transcoder::stream_url::StreamUrlResolver>>() {
        Some(resolver) => Transcoder::with_request(request, resolver.as_ref()).await,
        None => Transcoder::new(request).await,
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

/// The window headers both answers carry — the HEAD probe and the streamed
/// GET alike (issue #183). The window itself is the request module's derived
/// [`TranscodeWindow`] (issue #184); this helper only shapes it into
/// headers.
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

    // The deployment knobs the request is stated with: the ffmpeg timeout
    // (the construction deadline derives from it) and the target codec (the
    // answer's content type reads from it). Everything else — the window,
    // the seek, the byte counts, the argv, the deadline — derives inside the
    // request module (issue #184); what is left here is only shaping.
    let timeout_in_seconds = conf()
        .get(ConfName::FfmpegTimeoutSeconds)
        .unwrap()
        .parse()
        .unwrap();
    debug!("choosen timeout in seconds: {timeout_in_seconds}");

    let codec = conf().get(ConfName::AudioCodec).unwrap().into();
    let request = match TranscodeRequest::new(
        stream_url.clone(),
        duration_secs,
        bitrate,
        req.headers().get("Range"),
        timeout_in_seconds,
        codec,
    ) {
        Ok(request) => request,
        Err(TranscodeRequestError::MalformedRange(e)) => return HttpResponse::BadRequest().body(e),
        Err(TranscodeRequestError::RangeNotSatisfiable) => {
            return HttpResponse::RangeNotSatisfiable().finish()
        }
    };
    debug!("seconds: {duration_secs}, bitrate: {bitrate}");

    if req.method() == http::Method::HEAD {
        // The HEAD probe derives its answer from the request's window alone
        // — no resolution, no staging, no transcoder (issue #183).
        return insert_window_headers(HttpResponse::Ok(), &request.window)
            .content_type(request.codec.get_mime_type_str())
            .finish();
    }

    match tokio::time::timeout(
        // the construction deadline is the request's own derivation
        // (issue #184)
        request.construction_deadline(),
        build_transcoder(&req, &request),
    )
    .await
    {
        Ok(Ok(transcoder)) => {
            let stream = transcoder.get_transcode_stream();

            let response_builder = if request.window.partial {
                HttpResponse::PartialContent()
            } else {
                HttpResponse::Ok()
            };

            insert_window_headers(response_builder, &request.window)
                .content_type(request.codec.get_mime_type_str())
                .no_chunking((request.window.expected_bytes).try_into().unwrap())
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

    // The range-parse and window-decision pins live in the transcoder's
    // request module (issue #184), beside the core they pin. What lives here
    // is the HTTP shaping and the route-level behavior suite (issue #183).

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

    use crate::test_support::{
        env_lock, staging_test_limits, EnvPin, MediaFixture, OriginProvider, TempDir,
    };
    use crate::transcoder::media_sizing::streamable_bytes;
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
    /// honest about what they describe. The formula's one owner is the
    /// transcoder's media-sizing module (issue #184, ADR-0002).
    #[test]
    fn route_fixture_arithmetic_matches_the_shared_formula() {
        assert_eq!(
            streamable_bytes(ROUTE_DURATION_SECS as u64, ROUTE_BITRATE_KBIT as u64),
            ROUTE_TOTAL_BYTES as u64
        );
    }

    // The decision core's unit pins (issue #183) live in the transcoder's
    // request module (issue #184), beside the core they pin.

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
