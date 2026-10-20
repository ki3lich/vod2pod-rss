//! The YouTube provider: its `MediaProvider` surface and the resolved
//! stream-URL cache. The trait's interface is the dispatch seam the rest of
//! the system sees; the churn concentrates in the modules beside it:
//!
//! - [`quota`]: quota typing — the breaker identity, the reset time, and the
//!   typed `QuotaExhausted` mapping
//! - [`identity`]: the Canonical Feed Key derivation and the yt-dlp
//!   handle→channel-id resolution every path shares
//! - [`episode`]: the one Episode Metadata shaper both rung-producing paths
//!   call
//! - [`api`]: the metered YouTube Data API path
//! - [`atom`]: the quota-free atom path

mod api;
mod atom;
mod episode;
mod identity;
mod quota;

use async_trait::async_trait;
#[allow(unused_imports)]
use cached::macros::concurrent_cached;
#[allow(unused_imports)]
use cached::{AsyncRedisCache, ConcurrentCachedAsync};
use chrono::{DateTime, Utc};
use eyre::eyre;
use log::{debug, info, warn};
use regex::Regex;
use reqwest::Url;
use rss::ImageBuilder;

use crate::configs::{conf, Conf, ConfName};
use crate::provider::yt_dlp;
use crate::provider::{self, GeneratedFeed, MediaProvider};

pub struct YoutubeProvider;

#[async_trait]
impl MediaProvider for YoutubeProvider {
    async fn generate_rss_feed(&self, channel_url: Url) -> eyre::Result<GeneratedFeed> {
        let youtube_api_key = conf().get(ConfName::YoutubeApiKey).ok();

        match youtube_api_key {
            Some(api_key) => {
                info!(
                    "starting youtube feed generation for {} with API key",
                    channel_url
                );
                let mut feed_builder = provider::build_default_rss_structure();

                let id = api::id_from_url(&channel_url).await?;

                let filter_shorts = conf()
                    .get(ConfName::YoutubeFilterShorts)
                    .unwrap_or_else(|_| "false".to_string())
                    .eq_ignore_ascii_case("true");

                let mut fetched = api::fetch_from_api(id, api_key, filter_shorts)
                    .await
                    .map_err(quota::youtube_quota_typed)?;

                feed_builder.description(fetched.channel.description);
                feed_builder.title(fetched.channel.title);
                feed_builder.language(fetched.channel.language.take());
                let mut image_builder = ImageBuilder::default();
                image_builder.url(
                    fetched
                        .channel
                        .itunes_ext
                        .clone()
                        .and_then(|it| it.image)
                        .unwrap_or_default(),
                );
                feed_builder.image(Some(image_builder.build()));
                feed_builder.itunes_ext(fetched.channel.itunes_ext.take());
                feed_builder.link(fetched.channel.link);

                feed_builder.items(fetched.items);

                Ok(GeneratedFeed {
                    body: feed_builder.build().to_string(),
                    probe_state: Some(fetched.probe_state),
                    quota_units: Some(fetched.api_requests as u64),
                })
            }
            None => {
                info!(
                    "starting youtube feed generation for {} using atom feed",
                    channel_url
                );
                let body = atom::generate_rss_feed_via_atom(channel_url).await?;
                Ok(GeneratedFeed {
                    body,
                    probe_state: None,
                    quota_units: None,
                })
            }
        }
    }

    async fn canonical_feed_id(&self, channel_url: &Url) -> Option<String> {
        identity::canonical_yt_feed_id(channel_url).await
    }

    async fn probe_feed_freshness(
        &self,
        _channel_url: &Url,
        state: &provider::FeedProbeState,
    ) -> eyre::Result<bool> {
        // the probe fetches the exact page shape the conversion's pagination
        // recorded with its snapshot; state checks come before the config read
        // so a state without the recorded shape errors without touching the API
        let probe_target = state
            .probe_target
            .as_deref()
            .ok_or_else(|| eyre!("probe state has no probe target"))?;
        let page_size = state
            .probe_page_size
            .ok_or_else(|| eyre!("probe state has no probe page size"))?;

        let api_key = conf().get(ConfName::YoutubeApiKey)?;
        let page = api::fetch_probe_page(probe_target, page_size, &api_key).await?;
        info!("quota: ~1 unit consumed probing freshness of playlist {probe_target}");
        Ok(api::probe_state_unchanged(
            state,
            page.total_results,
            &page.first_page_ids,
            page.newest_published_at,
        ))
    }

    async fn generate_rss_feed_quota_free(&self, channel_url: Url) -> eyre::Result<String> {
        atom::generate_rss_feed_via_atom(channel_url).await
    }

    fn quota_breaker_key(&self) -> Option<String> {
        Some(quota::YT_QUOTA_BREAKER_KEY.to_string())
    }

    fn next_quota_reset(&self, now_utc: DateTime<Utc>) -> Option<DateTime<Utc>> {
        Some(quota::next_quota_reset(now_utc))
    }

    fn quota_usage_day(&self, now_utc: DateTime<Utc>) -> Option<String> {
        Some(quota::quota_usage_day(now_utc))
    }

    fn streams_direct_media(&self) -> bool {
        // yt-dlp resolves to a googlevideo file URL: the media bytes
        // themselves.
        true
    }

    async fn get_stream_url(&self, media_url: &Url) -> eyre::Result<Url> {
        get_youtube_stream_url(media_url).await
    }

    async fn evict_stream_url_cache(&self, media_url: &Url) -> eyre::Result<()> {
        evict_cached_yt_stream_url(media_url).await
    }

    fn domain_whitelist_regexes(&self) -> Vec<Regex> {
        let youtube_whitelist = vec![
            regex::Regex::new(r"^(https://)?.*\.youtube\.com/").unwrap(),
            regex::Regex::new(r"^(https://)?youtube\.com/").unwrap(),
            regex::Regex::new(r"^(https://)?youtu\.be/").unwrap(),
            regex::Regex::new(r"^(https://)?.*\.youtu\.be/").unwrap(),
            regex::Regex::new(r"^(https://)?.*\.googlevideo\.com/").unwrap(),
        ];

        #[cfg(not(test))]
        return youtube_whitelist;
        #[cfg(test)] //this will allow test to use localhost ad still work
        return [
            youtube_whitelist,
            vec![regex::Regex::new(r"^http://127\.0\.0\.1:9870").unwrap()],
        ]
        .concat();
    }
}

/// Redis key prefix for the cached resolved youtube stream URL, passed to
/// [`build_yt_stream_url_cache`].
const YT_STREAM_URL_CACHE_PREFIX: &str = "cached_yt_stream_url=";

/// Build the Redis-backed cache for [`get_youtube_stream_url`].
///
/// Single construction point shared by the `#[concurrent_cached]` macro's
/// `create` block and the eviction path in [`evict_cached_yt_stream_url`], so
/// writer and evictor always operate on identically-configured stores.
async fn build_yt_stream_url_cache() -> AsyncRedisCache<Url, Url> {
    AsyncRedisCache::builder(YT_STREAM_URL_CACHE_PREFIX)
        .ttl(std::time::Duration::from_secs(900))
        .refresh_on_hit(false)
        // Auto-reconnect: these singletons hold one connection for the
        // process lifetime, and without the manager a single Redis blip
        // poisoned it with "broken pipe" until the next app restart (the
        // 2026-10-19 downloads outage). See ADR 0003.
        .connection_manager(true)
        .connection_string(&conf().get(ConfName::RedisUrl).unwrap())
        .build()
        .await
        .expect("get_youtube_stream_url cache")
}

#[concurrent_cached(
    name = "YT_STREAM_URL_CACHE",
    map_error = r##"|e| eyre::Error::new(e)"##,
    ty = "AsyncRedisCache<Url, Url>",
    create = r##" build_yt_stream_url_cache().await "##
)]
async fn get_youtube_stream_url(url: &Url) -> eyre::Result<Url> {
    let extra_args: Vec<String> =
        serde_json::from_str(conf().get(ConfName::YoutubeYtDlpExtraArgs)?.as_str()).map_err(|_| eyre!(r#"failed to parse YOUTUBE_YT_DLP_GET_URL_EXTRA_ARGS allowed syntax is ["arg1#", "arg2", "arg3", ...]"#))?;
    yt_dlp::stream_url(url, &extra_args).await
}

/// Best-effort eviction of the cached googlevideo stream URL for `watch_url`.
///
/// Called when a stream URL is found to be unreachable so the next request
/// re-resolves via yt-dlp (the upstream CDN edge may have rotated to a working
/// node). Removal goes through the crate's own `async_cache_remove` on the
/// same store instance the writer uses, so the Redis key layout can never
/// drift between writer and evictor. Always returns `Ok(())`: a Redis failure
/// must not mask the original transcode error.
async fn evict_cached_yt_stream_url(watch_url: &Url) -> eyre::Result<()> {
    let cache = YT_STREAM_URL_CACHE
        .get_or_init(build_yt_stream_url_cache)
        .await;
    match cache.async_cache_remove(watch_url).await {
        Ok(Some(_)) => info!("evicted cached youtube stream url for {watch_url}"),
        Ok(None) => debug!("no cached youtube stream url to evict for {watch_url}"),
        Err(e) => warn!("failed to evict cached youtube stream url for {watch_url}: {e}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_log::test;

    #[test(tokio::test)]
    async fn test_fetch_youtube_channel_by_name_requires_api_key() {
        let provider = YoutubeProvider;
        let Ok(_api_key) = conf().get(ConfName::YoutubeApiKey) else {
            panic!("to run this test you need to set an api key for youtube.");
        };

        let result = provider
            .generate_rss_feed(Url::parse("https://www.youtube.com/@LegalEagle").unwrap())
            .await;
        assert!(result.is_ok());

        let feed = result.unwrap();
        let channel = rss::Channel::read_from(feed.body.as_bytes()).unwrap();
        assert!(channel.items.len() > 50);
        for item in &channel.items {
            assert!(item.title.is_some());
            assert!(item.description.is_some());
        }
    }

    #[tokio::test]
    async fn test_probe_without_recorded_page_size_fails_into_full_conversion() {
        // a state captured before the shape was recorded cannot be probed
        // trustworthily; the ladder falls through to a full conversion, which
        // re-captures the shape. No API key is read: the check comes first.
        let state = provider::FeedProbeState {
            probe_target: Some("PLxyz".to_string()),
            ..Default::default()
        };
        let err = YoutubeProvider
            .probe_feed_freshness(
                &Url::parse("https://www.youtube.com/playlist?list=PLxyz").unwrap(),
                &state,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("probe page size"));
    }
}
