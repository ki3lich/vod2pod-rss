use std::{
    collections::HashMap,
    net::TcpListener,
    time::{Duration, Instant},
};

use actix_web::{
    dev::Server, guard, http, middleware, web, App, HttpRequest, HttpResponse, HttpServer,
};
use chrono::{DateTime, Utc};
use log::{debug, error, info, warn};
use regex::Regex;
use serde::Deserialize;
use url::Url;

use crate::{
    configs::{conf, conf_u64, Conf, ConfName},
    feed_cache,
    provider::{self, MediaProvider},
    rss_transcodizer,
    transcoder::{FfmpegParameters, Transcoder},
};

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

    let start_time = Instant::now();

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

    let provider = provider::from(&parsed_url);

    if !provider
        .domain_whitelist_regexes()
        .iter()
        .any(|r| r.is_match(parsed_url.as_ref()))
    {
        error!("supplied url ({parsed_url}) not in whitelist (whitelist is needed to prevent SSRF attack)");
        return HttpResponse::Forbidden().body("scheme and host not in whitelist");
    }

    // Canonical Feed Key: the provider identity when it can be determined
    // cheaply (e.g. the youtube playlist/channel id), so different spellings
    // of the same feed share one cache entry; the normalized source url
    // otherwise
    let canonical_id = provider
        .canonical_feed_id(&parsed_url)
        .await
        .unwrap_or_else(|| feed_cache::canonicalize_url(&parsed_url));
    debug!("canonical feed key for {parsed_url}: {canonical_id}");

    let Ok(mut redis) = crate::get_redis_client().await else {
        error!("could not get redis client");
        return HttpResponse::InternalServerError().finish();
    };

    // Degradation Ladder rung 0: a fresh copy is served without contacting
    // the provider at all; a stale copy (Stale Window) is served while the
    // feed is revalidated in the background (stale-while-revalidate)
    let lookup = feed_cache::lookup(&mut redis, &canonical_id).await;
    let decision = lookup_decision(&lookup);
    if decision != feed_cache::ServeDecision::Miss {
        return serve_lookup_hit(
            &req,
            lookup,
            decision,
            &parsed_url,
            &canonical_id,
            should_transcode,
            &transcode_service_url,
        );
    }

    // nothing usable cached: a Feed Conversion is needed. Requests for the
    // same feed are serialized so a poll burst shares one upstream pass; the
    // cache is re-checked after waiting because the previous slot holder may
    // have refreshed it already.
    let _regen_slot = feed_cache::begin_regeneration(&canonical_id).await;
    let lookup = feed_cache::lookup(&mut redis, &canonical_id).await;
    let decision = lookup_decision(&lookup);
    if decision != feed_cache::ServeDecision::Miss {
        info!("re-checked cache after waiting for a concurrent regeneration of {canonical_id}");
        return serve_lookup_hit(
            &req,
            lookup,
            decision,
            &parsed_url,
            &canonical_id,
            should_transcode,
            &transcode_service_url,
        );
    }

    // Quota Breaker: when THIS provider's quota is known to be exhausted,
    // metered calls are skipped entirely and the ladder degrades right away
    let breaker_open = match provider.quota_breaker_key().as_deref() {
        Some(key) => feed_cache::quota_breaker_open(&mut redis, key).await,
        None => None,
    };
    if let Some(reset) = breaker_open {
        info!(
            "quota breaker open until {}, degrading to quota-free generation for {parsed_url}",
            DateTime::<Utc>::from_timestamp(reset, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default()
        );
        return degrade_to_quota_free(
            &provider,
            &parsed_url,
            &canonical_id,
            &mut redis,
            &req,
            should_transcode,
            &transcode_service_url,
        )
        .await;
    }

    // Degradation Ladder rung 1: full conversion
    match regenerate_and_store(&provider, &parsed_url, &canonical_id, &mut redis).await {
        Ok(entry) => {
            debug!(
                "rss generation took {} seconds",
                (Instant::now() - start_time).as_secs_f32()
            );
            // prefer the cache read so the fresh marker (and its max-age)
            // reflects what was just stored; fall back to the generated body
            // when the cache write failed
            let lookup = feed_cache::lookup(&mut redis, &canonical_id).await;
            match lookup.entry {
                Some(entry) => serve_feed(
                    &req,
                    entry,
                    should_transcode,
                    &transcode_service_url,
                    lookup
                        .fresh_marker_ttl
                        .unwrap_or(fresh_ttl_secs() as i64),
                ),
                None => serve_feed(
                    &req,
                    entry,
                    should_transcode,
                    &transcode_service_url,
                    fresh_ttl_secs() as i64,
                ),
            }
        }
        Err(e) if feed_cache::is_quota_exceeded(&e) => {
            error!("provider quota exhausted for {parsed_url}:\n{e}");
            if let Some(key) = provider.quota_breaker_key() {
                open_quota_breaker(&mut redis, &key).await;
            }
            // Degradation Ladder rung 2: quota-free generation
            degrade_to_quota_free(
                &provider,
                &parsed_url,
                &canonical_id,
                &mut redis,
                &req,
                should_transcode,
                &transcode_service_url,
            )
            .await
        }
        Err(e) => {
            error!("could not generate rss feed for {parsed_url}:\n{e}");
            if feed_cache::is_quota_exceeded(&e) {
                if let Some(key) = provider.quota_breaker_key() {
                    open_quota_breaker(&mut redis, &key).await;
                }
            }
            // Degradation Ladder rung 2: quota-free generation, on any
            // conversion failure (quota exhaustion included); the fallback
            // itself failing surfaces the error to the client
            degrade_to_quota_free(
                &provider,
                &parsed_url,
                &canonical_id,
                &mut redis,
                &req,
                should_transcode,
                &transcode_service_url,
            )
            .await
        }
    }
}

/// Max-Age sent when serving a stale copy: tell the client to come back soon,
/// the background revalidation is likely done by then.
const STALE_SERVE_MAX_AGE_SECS: i64 = 60;

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

/// Freshness classification of a cache lookup.
fn lookup_decision(lookup: &feed_cache::Lookup) -> feed_cache::ServeDecision {
    feed_cache::classify_freshness(
        lookup.entry.is_some(),
        lookup.fresh_marker_ttl.is_some(),
        lookup
            .entry
            .as_ref()
            .map(|entry| entry.generated_at)
            .unwrap_or(0),
        feed_cache::unix_now(),
        max_fresh_period_secs() as i64,
        stale_max_age_secs() as i64,
    )
}

/// Serve a cache hit classified as Fresh or Stale. A Stale hit also kicks off
/// the background revalidation. Miss must never be passed here.
fn serve_lookup_hit(
    req: &HttpRequest,
    lookup: feed_cache::Lookup,
    decision: feed_cache::ServeDecision,
    parsed_url: &Url,
    canonical_id: &str,
    should_transcode: bool,
    transcode_service_url: &Url,
) -> HttpResponse {
    let Some(entry) = lookup.entry else {
        // classification guarantees an entry for Fresh/Stale; degrade to an
        // error response instead of panicking if that invariant ever breaks
        error!("cache hit classified {decision:?} but no entry found for {canonical_id}");
        return HttpResponse::Conflict().finish();
    };
    match decision {
        feed_cache::ServeDecision::Fresh => {
            info!("serving cached rss feed for {parsed_url}");
            let max_age = lookup
                .fresh_marker_ttl
                .unwrap_or(fresh_ttl_secs() as i64);
            serve_feed(req, entry, should_transcode, transcode_service_url, max_age)
        }
        feed_cache::ServeDecision::Stale => {
            info!("serving stale rss feed for {parsed_url}, revalidating in background");
            spawn_revalidation(parsed_url.clone(), canonical_id.to_string());
            serve_feed(
                req,
                entry,
                should_transcode,
                transcode_service_url,
                STALE_SERVE_MAX_AGE_SECS,
            )
        }
        feed_cache::ServeDecision::Miss => {
            error!("serve_lookup_hit called with Miss for {canonical_id}");
            HttpResponse::InternalServerError().finish()
        }
    }
}

/// Inject the per-request customizations into the raw cached body and answer
/// the client, honoring conditional GET (If-None-Match / If-Modified-Since).
fn serve_feed(
    req: &HttpRequest,
    entry: feed_cache::CachedFeed,
    should_transcode: bool,
    transcode_service_url: &Url,
    max_age_secs: i64,
) -> HttpResponse {
    match rss_transcodizer::inject_vod2pod_customizations(
        entry.body,
        should_transcode.then(|| transcode_service_url.clone()),
    ) {
        Ok(body) => conditional_response(req.headers(), body, entry.generated_at, max_age_secs),
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
    let etag = feed_cache::etag_of(&body);
    let cache_control = format!("public, max-age={max_age_secs}");

    // RFC 7232: If-None-Match takes precedence over If-Modified-Since
    let client_has_current = headers
        .get(http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|if_none_match| feed_cache::etag_matches(if_none_match, &etag))
        .unwrap_or_else(|| {
            headers
                .get(http::header::IF_MODIFIED_SINCE)
                .and_then(|value| value.to_str().ok())
                .map(|if_modified_since| {
                    feed_cache::http_date_at_or_after(if_modified_since, generated_at)
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
        .content_type("application/xml")
        .insert_header((http::header::ETAG, format!("\"{etag}\"")))
        .insert_header((http::header::CACHE_CONTROL, cache_control))
        .insert_header((http::header::LAST_MODIFIED, feed_cache::http_date(generated_at)))
        .body(body)
}

/// Full Feed Conversion, then store the raw body together with the Freshness
/// Probe state it was generated from. Returns the generated entry so the
/// caller can serve it even when the cache is unavailable.
async fn regenerate_and_store(
    provider: &provider::Provider,
    source_url: &Url,
    canonical_id: &str,
    con: &mut redis::aio::MultiplexedConnection,
) -> eyre::Result<feed_cache::CachedFeed> {
    let generated = provider.generate_rss_feed(source_url.clone()).await?;

    let entry = feed_cache::CachedFeed {
        body: generated.body,
        generated_at: feed_cache::unix_now(),
        probe_state: generated.probe_state,
        degraded: false,
    };
    if let Err(e) = feed_cache::store(
        con,
        canonical_id,
        &entry,
        feed_cache::jittered_ttl(fresh_ttl_secs()),
        stale_max_age_secs(),
    )
    .await
    {
        // the client is served anyway; losing the cache entry only costs quota
        warn!("could not store feed {canonical_id} in cache: {e}");
    }

    if let Some(units) = generated.quota_units {
        match feed_cache::bump_quota_usage(con, units).await {
            Ok(total) => info!("quota: ~{total} units consumed today (estimate)"),
            Err(e) => warn!("could not record quota usage estimate: {e}"),
        }
    }

    Ok(entry)
}

/// Fresh TTL given to a Degraded Feed: short on purpose, so the next
/// revalidation replaces it with a full feed as soon as possible (e.g. right
/// after the quota reset) instead of serving the limited feed for a full hour.
const DEGRADED_FRESH_TTL_SECS: u64 = 300;

/// Degradation Ladder rung: produce a feed without metered API usage (the
/// youtube public atom feed) and cache it as a Degraded Feed. Used when the
/// Feed Conversion failed (quota exhaustion included) and no cached copy can
/// serve the client.
async fn degrade_to_quota_free(
    provider: &provider::Provider,
    source_url: &Url,
    canonical_id: &str,
    con: &mut redis::aio::MultiplexedConnection,
    req: &HttpRequest,
    should_transcode: bool,
    transcode_service_url: &Url,
) -> HttpResponse {
    warn!("degrading to quota-free feed generation for {source_url}");
    match provider.generate_rss_feed_quota_free(source_url.clone()).await {
        Ok(body) => {
            let entry = feed_cache::CachedFeed {
                body,
                generated_at: feed_cache::unix_now(),
                probe_state: None,
                degraded: true,
            };
            if let Err(e) = feed_cache::store(
                con,
                canonical_id,
                &entry,
                DEGRADED_FRESH_TTL_SECS,
                stale_max_age_secs(),
            )
            .await
            {
                warn!("could not store degraded feed {canonical_id} in cache: {e}");
            }
            serve_feed(
                req,
                entry,
                should_transcode,
                transcode_service_url,
                DEGRADED_FRESH_TTL_SECS as i64,
            )
        }
        Err(fallback_err) => {
            error!("quota-free fallback failed for {source_url}:\n{fallback_err}");
            HttpResponse::Conflict().finish()
        }
    }
}

/// Record that a provider's quota is exhausted until its next reset (midnight
/// pacific for youtube), so no request pays the failing-call cost until then.
async fn open_quota_breaker(con: &mut redis::aio::MultiplexedConnection, breaker_key: &str) {
    let reset = feed_cache::next_quota_reset(Utc::now());
    match feed_cache::open_quota_breaker(con, breaker_key, reset.timestamp()).await {
        Ok(()) => warn!("quota breaker {breaker_key} open until {}", reset.to_rfc3339()),
        Err(e) => warn!("could not persist quota breaker: {e}"),
    }
}

/// Background revalidation of a stale feed (stale-while-revalidate): probe
/// first (~1 quota unit), full conversion only when needed. Failures keep the
/// stale body cached so clients keep being served.
async fn background_revalidate(source_url: Url, canonical_id: String) {
    let Some(_regen_slot) = feed_cache::try_begin_regeneration(&canonical_id).await else {
        debug!("revalidation of {canonical_id} already in flight, skipping");
        return;
    };
    let Ok(mut redis) = crate::get_redis_client().await else {
        warn!("could not get redis client for background revalidation of {canonical_id}");
        return;
    };

    let provider = provider::from(&source_url);
    let breaker_open = match provider.quota_breaker_key().as_deref() {
        Some(key) => feed_cache::quota_breaker_open(&mut redis, key).await,
        None => None,
    };
    if breaker_open.is_some() {
        info!("quota breaker open, skipping background revalidation of {canonical_id}");
        return;
    }

    let Some(cached) = feed_cache::load(&mut redis, &canonical_id).await else {
        return;
    };

    let age = feed_cache::unix_now() - cached.generated_at;
    // probes may only renew a feed while it is within the Fresh Period;
    // degraded feeds are never probed, they must be replaced by a real
    // conversion as soon as possible
    let probeable = age < max_fresh_period_secs() as i64 && !cached.degraded;

    if probeable {
        if let Some(state) = cached.probe_state.clone() {
            match provider.probe_feed_freshness(&source_url, &state).await {
                Ok(true) => {
                    info!("feed {canonical_id} unchanged upstream, renewing freshness after probe");
                    if let Err(e) = feed_cache::bump_quota_usage(&mut redis, 1).await {
                        warn!("could not record quota usage estimate: {e}");
                    }
                    // the fresh marker must not outlive the Fresh Period
                    let remaining_fresh =
                        (max_fresh_period_secs() as i64 - age).min(fresh_ttl_secs() as i64);
                    if let Err(e) = feed_cache::renew_freshness(
                        &mut redis,
                        &canonical_id,
                        remaining_fresh.max(1) as u64,
                        stale_max_age_secs(),
                    )
                    .await
                    {
                        warn!("could not renew freshness of {canonical_id}: {e}");
                    }
                    return;
                }
                Ok(false) => info!("feed {canonical_id} changed upstream, regenerating"),
                Err(e) => warn!("freshness probe failed for {canonical_id} ({e}), regenerating"),
            }
        }
    } else if age >= max_fresh_period_secs() as i64 {
        info!("feed {canonical_id} reached the fresh period limit, regenerating fully");
    }

    if let Err(e) = regenerate_and_store(&provider, &source_url, &canonical_id, &mut redis).await {
        if feed_cache::is_quota_exceeded(&e) {
            warn!("provider quota exhausted during background revalidation of {canonical_id}");
            if let Some(key) = provider.quota_breaker_key() {
                open_quota_breaker(&mut redis, &key).await;
            }
        } else {
            error!("background revalidation of {canonical_id} failed:\n{e}");
        }
        // the stale body is kept on purpose, it keeps serving clients
    }
}

fn spawn_revalidation(source_url: Url, canonical_id: String) {
    actix_rt::spawn(background_revalidate(source_url, canonical_id));
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
    let total_streamable_bytes = (duration_secs * bitrate * 1000) / 8;
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
    fn test_conditional_response_serves_304_on_if_none_match() {
        let body = "<rss>feed body</rss>";
        let etag = feed_cache::etag_of(body);

        let mut headers = http::header::HeaderMap::new();
        headers.insert(
            http::header::IF_NONE_MATCH,
            http::header::HeaderValue::from_str(&format!("\"{etag}\"")).unwrap(),
        );

        let response = conditional_response(&headers, body.to_string(), 1_758_424_055, 3600);

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
            Some(format!("\"{}\"", feed_cache::etag_of(body)).as_str())
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok()),
            Some(feed_cache::http_date(1_758_424_055).as_str())
        );
    }

    #[test]
    fn test_conditional_response_304_on_if_modified_since() {
        let generated_at = 1_758_424_055;

        let mut headers = http::header::HeaderMap::new();
        headers.insert(
            http::header::IF_MODIFIED_SINCE,
            http::header::HeaderValue::from_str(&feed_cache::http_date(generated_at)).unwrap(),
        );

        let response = conditional_response(
            &headers,
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
            http::header::HeaderValue::from_str(&feed_cache::http_date(generated_at)).unwrap(),
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
