use std::{collections::HashMap, net::TcpListener, sync::Arc, time::Duration};

use actix_web::{
    dev::Server, guard, http, middleware, web, App, HttpRequest, HttpResponse, HttpServer,
};
use log::{debug, error, info, warn};
use regex::Regex;
use serde::Deserialize;
use url::Url;

use crate::{
    configs::{conf, conf_u64, Conf, ConfName},
    feed_serving::{self, FeedServing, ServedFeed, ServingPolicy},
    provider::{self, MediaProvider},
    rss_transcodizer,
    transcoder::{FfmpegParameters, Transcoder},
};

mod headers;

pub fn spawn_server(listener: TcpListener) -> eyre::Result<Server> {
    let root = conf().get(ConfName::SubfolderPath).unwrap();
    Ok(HttpServer::new(move || {
        App::new()
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
            )
    })
    .listen(listener)?
    .run())
}

async fn health() -> HttpResponse {
    HttpResponse::Ok().finish()
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

    let provider = Arc::new(provider::from(&parsed_url));

    if !provider
        .domain_whitelist_regexes()
        .iter()
        .any(|r| r.is_match(parsed_url.as_ref()))
    {
        error!("supplied url ({parsed_url}) not in whitelist (whitelist is needed to prevent SSRF attack)");
        return HttpResponse::Forbidden().body("scheme and host not in whitelist");
    }

    let Ok(redis) = crate::get_redis_client().await else {
        error!("could not get redis client");
        return HttpResponse::InternalServerError().finish();
    };

    // the Feed Serving module owns the Degradation Ladder, stale-while-
    // revalidate included; the adapter reads the policy numbers from config
    // and injects them at construction (the module itself reads no
    // environment configuration)
    let cache = Arc::new(feed_serving::RedisFeedCache::new(redis.clone()));
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

fn parse_range_header(
    content_range_str: &str,
    bytes_count: usize,
) -> eyre::Result<(usize, usize, usize)> {
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

    if bytes_count == 0 {
        error!("The requested Rage header with a length of 0 is invalid: {content_range_str}");
        return Err(eyre::eyre!(
            "The requested Rage header with a length of 0 is invalid: {content_range_str}"
        ));
    }
    let mut end = bytes_count - 1;
    if let Some(x) = captures.name("end") {
        end = x.as_str().parse()?;
    }

    if end == start {
        return Err(eyre::eyre!(
            "The requested Rage header with a length of 0 is invalid: {content_range_str}"
        ));
    }

    let expected = (end + 1) - start;

    Ok((start, end, expected))
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

    if !provider
        .domain_whitelist_regexes()
        .iter()
        .any(|r| r.is_match(stream_url.as_ref()))
    {
        error!("supplied url ({stream_url}) not in whitelist (whitelist is needed to prevent SSRF attack)");
        return HttpResponse::Forbidden().body("scheme and host not in whitelist");
    }

    // Range header parsing
    const DEFAULT_CONTENT_RANGE: &str = "0-";
    let content_range_str = match req.headers().get("Range") {
        Some(x) => x.to_str().unwrap_or_default(),
        None => DEFAULT_CONTENT_RANGE,
    };

    debug!("received content range {content_range_str}");

    let (start_bytes, end_bytes, expected_bytes) =
        match parse_range_header(content_range_str, total_streamable_bytes) {
            Ok((start, end, expected)) => (start, end, expected),
            Err(e) => return HttpResponse::BadRequest().body(e.to_string()),
        };

    debug!("requested content-range: bytes {start_bytes}-{end_bytes}/{total_streamable_bytes}");

    if start_bytes > end_bytes || start_bytes > total_streamable_bytes {
        return HttpResponse::RangeNotSatisfiable().finish();
    }

    let seek_secs =
        ((start_bytes as f32) / (total_streamable_bytes as f32)) * (duration_secs as f32);
    debug!("choosen seek_time: {seek_secs}");

    let timeout_in_seconds = conf()
        .get(ConfName::FfmpegTimeoutSeconds)
        .unwrap()
        .parse()
        .unwrap();
    debug!("choosen timeout in seconds: {timeout_in_seconds}");

    let codec = conf().get(ConfName::AudioCodec).unwrap().into();
    let ffmpeg_paramenters = FfmpegParameters {
        seek_time: seek_secs,
        url: stream_url.clone(),
        audio_codec: codec,
        bitrate_kbit: bitrate,
        max_rate_kbit: bitrate * 30,
        expected_bytes_count: expected_bytes,
        timeout_in_seconds,
    };
    debug!("seconds: {duration_secs}, bitrate: {bitrate}");

    if req.method() == http::Method::HEAD {
        return HttpResponse::Ok()
            .insert_header(("Accept-Ranges", "bytes"))
            .insert_header((
                "Content-Range",
                format!("bytes {start_bytes}-{end_bytes}/{total_streamable_bytes}"),
            ))
            .content_type(codec.get_mime_type_str())
            .finish();
    }

    match tokio::time::timeout(
        Duration::from_secs(timeout_in_seconds.try_into().unwrap_or(300)),
        Transcoder::new(&ffmpeg_paramenters),
    )
    .await
    {
        Ok(Ok(transcoder)) => {
            let stream = transcoder.get_transcode_stream();

            let mut response_builder = if ffmpeg_paramenters.seek_time <= 0.1 {
                HttpResponse::Ok()
            } else {
                HttpResponse::PartialContent()
            };

            response_builder
                .insert_header(("Accept-Ranges", "bytes"))
                .insert_header((
                    "Content-Range",
                    format!("bytes {start_bytes}-{end_bytes}/{total_streamable_bytes}"),
                ))
                .content_type(codec.get_mime_type_str())
                .no_chunking((expected_bytes).try_into().unwrap())
                .streaming(stream)
        }
        Ok(Err(e)) => HttpResponse::ServiceUnavailable().body(e.to_string()),
        Err(_) => HttpResponse::ServiceUnavailable().body("transcoder initialization timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_start_and_end_start_to_end() {
        let content_range_str = "bytes=0-99";
        let bytes_count = 100;
        let (start, end, expected) = parse_range_header(content_range_str, bytes_count).unwrap();
        assert_eq!((start, end, expected), (0, 99, 100));
    }

    #[test]
    fn test_get_start_and_end_middle1_to_middle2() {
        let content_range_str = "bytes=50-199";
        let bytes_count = 200;
        let (start, end, expected) = parse_range_header(content_range_str, bytes_count).unwrap();
        assert_eq!((start, end, expected), (50, 199, 150));
    }

    #[test]
    fn test_get_start_and_end_middle_to_undefined() {
        let content_range_str = "bytes=100-";
        let bytes_count = 200;
        let (start, end, expected) = parse_range_header(content_range_str, bytes_count).unwrap();
        assert_eq!((start, end, expected), (100, 199, 100));
    }

    #[test]
    fn test_get_start_and_end_start_to_undefined() {
        let content_range_str = "bytes=0-";
        let bytes_count = 200;
        let (start, end, expected) = parse_range_header(content_range_str, bytes_count).unwrap();
        assert_eq!((start, end, expected), (0, 199, 200));
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
}
