#[allow(unused_imports)]
use cached::macros::concurrent_cached;
#[allow(unused_imports)]
use cached::{AsyncRedisCache, ConcurrentCachedAsync};
use feed_rs::model::Feed;
use google_apis_common::NoToken;
use google_youtube3::{
    api::{self, PlaylistItem},
    hyper_rustls, YouTube,
};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::America::Los_Angeles;
use eyre::eyre;
use futures::stream::{self, StreamExt};
use log::{debug, info, warn};
use regex::Regex;
use reqwest::Url;
use rss::{
    extension::itunes::{ITunesChannelExtensionBuilder, ITunesItemExtensionBuilder},
    Channel, ChannelBuilder, GuidBuilder, ImageBuilder, Item, ItemBuilder,
};

use crate::{
    configs::{conf, Conf, ConfName},
    episode_identity, provider,
};

use super::duration::{format_duration, parse_duration};
use super::yt_dlp;
use super::{GeneratedFeed, MediaProvider};

pub struct YoutubeProvider;

/// Duration threshold for YouTube Shorts (180 seconds = 3 minutes)
const SHORTS_THRESHOLD_SECONDS: u64 = 180;

enum IdType {
    Playlist(String),
    Channel(String),
}

/// Quota breaker key of the YouTube provider: the identity under which the
/// Feed Serving module's cache adapters track this provider's quota
/// exhaustion. Provider-owned: the cache stores opaque breaker records.
const YT_QUOTA_BREAKER_KEY: &str = "vod2pod:quota_breaker:youtube";

/// The moment YouTube's daily quota resets: midnight America/Los_Angeles.
/// The provider's answer to "when does your quota reset"; pure function so
/// tests can exercise it.
fn next_quota_reset(now_utc: DateTime<Utc>) -> DateTime<Utc> {
    let pt_now = now_utc.with_timezone(&Los_Angeles);
    let next_day = pt_now.date_naive() + chrono::Duration::days(1);
    let midnight = next_day
        .and_hms_opt(0, 0, 0)
        .expect("midnight of any date exists");
    match Los_Angeles.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.with_timezone(&Utc)
        }
        // midnight is never skipped by US DST transitions; if that ever
        // changes, fall back to a conservative fixed PST offset (resets early
        // rather than late)
        chrono::LocalResult::None => {
            let approx = midnight - chrono::Duration::hours(8);
            DateTime::<Utc>::from_naive_utc_and_offset(approx, Utc)
        }
    }
}

/// Whether the upstream failure is the YouTube Data API refusing the call
/// because the project's daily quota is spent. YouTube's error vocabulary is
/// the provider's own concern: the rest of the system only ever sees the
/// typed `QuotaExhausted` conversion failure.
///
/// ceiling: detection is exactly as wide as the crate's JSON decoding. A
/// quota error whose body is not valid JSON arrives as `Error::Failure`,
/// carrying no structured reason to type against — identifying it would
/// require matching upstream error text, which the typed seam forbids — so
/// it degrades without opening the breaker.
fn is_youtube_quota_error(err: &eyre::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<google_youtube3::Error>(),
            Some(google_youtube3::Error::BadRequest(body))
                if is_quota_exceeded_body(body)
        )
    })
}

/// Whether a YouTube Data API error body carries the quota-exceeded reason.
fn is_quota_exceeded_body(body: &serde_json::Value) -> bool {
    body["error"]["errors"].as_array().is_some_and(|errors| {
        errors
            .iter()
            .any(|e| e["reason"].as_str() == Some("quotaExceeded"))
    })
}

/// Map a conversion failure to the typed `QuotaExhausted` case when the
/// upstream error is YouTube quota exhaustion; any other failure passes
/// through untouched.
fn youtube_quota_typed(err: eyre::Error) -> eyre::Error {
    if is_youtube_quota_error(&err) {
        eyre::Report::new(provider::QuotaExhausted::new(err))
    } else {
        err
    }
}

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

                let id = match channel_url.path() {
                    path if path.starts_with("/playlist") => {
                        let playlist_id = channel_url
                            .query_pairs()
                            .find(|(key, _)| key == "list")
                            .map(|(_, value)| value)
                            .ok_or_else(|| {
                                eyre::eyre!("Failed to parse playlist ID from URL: {}", channel_url)
                            })?;
                        IdType::Playlist(playlist_id.into())
                    }
                    path if path.starts_with("/channel/")
                        || path.starts_with("/user/")
                        || path.starts_with("/c/")
                        || path.starts_with("/@") =>
                    {
                        let url = find_yt_channel_url_with_c_id(&channel_url).await?;
                        let channel_id = url.path_segments().unwrap().next_back().unwrap();
                        IdType::Channel(channel_id.into())
                    }
                    _ => return Err(eyre!("unsupported youtube url")),
                };

                let filter_shorts = conf()
                    .get(ConfName::YoutubeFilterShorts)
                    .unwrap_or_else(|_| "false".to_string())
                    .eq_ignore_ascii_case("true");

                let mut fetched = fetch_from_api(id, api_key, filter_shorts)
                    .await
                    .map_err(youtube_quota_typed)?;

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
                let body = generate_rss_feed_via_atom(channel_url).await?;
                Ok(GeneratedFeed {
                    body,
                    probe_state: None,
                    quota_units: None,
                })
            }
        }
    }

    async fn canonical_feed_id(&self, channel_url: &Url) -> Option<String> {
        canonical_yt_feed_id(channel_url).await
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
        let page = fetch_probe_page(probe_target, page_size, &api_key).await?;
        info!("quota: ~1 unit consumed probing freshness of playlist {probe_target}");
        Ok(probe_state_unchanged(
            state,
            page.total_results,
            &page.first_page_ids,
            page.newest_published_at,
        ))
    }

    async fn generate_rss_feed_quota_free(&self, channel_url: Url) -> eyre::Result<String> {
        generate_rss_feed_via_atom(channel_url).await
    }

    fn quota_breaker_key(&self) -> Option<String> {
        Some(YT_QUOTA_BREAKER_KEY.to_string())
    }

    fn next_quota_reset(&self, now_utc: DateTime<Utc>) -> Option<DateTime<Utc>> {
        Some(next_quota_reset(now_utc))
    }

    fn quota_usage_day(&self, now_utc: DateTime<Utc>) -> Option<String> {
        Some(now_utc.with_timezone(&Los_Angeles).format("%F").to_string())
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

/// Video ids (in playlist position order) and the newest publication
/// timestamp (unix seconds) of one playlistItems page. Pure function so tests
/// can exercise it; the probe and the full conversion must derive identical
/// state from the same page.
fn first_page_snapshot(items: &[PlaylistItem]) -> (Vec<String>, Option<i64>) {
    let ids = items
        .iter()
        .filter_map(|i| i.snippet.as_ref()?.resource_id.as_ref()?.video_id.clone())
        .collect();
    let newest = items
        .iter()
        .filter_map(|i| i.snippet.as_ref().and_then(|s| s.published_at))
        .max()
        .map(|published_at| published_at.timestamp());
    (ids, newest)
}

/// Fingerprint of a playlist's source state: total item count plus the ordered
/// ids of the first page. Pure function so tests can exercise it.
fn build_source_fingerprint(total_results: Option<i32>, first_page_ids: &[String]) -> String {
    format!(
        "{}|{}",
        total_results.unwrap_or(-1),
        first_page_ids.join(",")
    )
}

/// The Freshness Probe state a Feed Conversion captures from the pagination's
/// own fetch: the first page's snapshot plus the exact page shape it was
/// captured from, as one record. A probe re-fetching the recorded shape
/// re-derives the snapshot the conversion saw; deriving the shape anywhere
/// else would let a pagination change silently turn every probe "changed".
/// Pure function so tests can exercise it.
fn probe_state_from_fetch(
    playlist_fetch: &PlaylistFetch,
    probe_target: &str,
) -> provider::FeedProbeState {
    provider::FeedProbeState {
        newest_published_at: playlist_fetch.newest_published_at,
        source_fingerprint: Some(build_source_fingerprint(
            playlist_fetch.total_results,
            &playlist_fetch.first_page_ids,
        )),
        probe_target: Some(probe_target.to_string()),
        probe_page_size: Some(playlist_fetch.first_page_size),
    }
}

/// Whether upstream still matches the state captured at Feed Conversion time.
/// Pure function so tests can exercise it.
fn probe_state_unchanged(
    state: &provider::FeedProbeState,
    total_results: Option<i32>,
    first_page_ids: &[String],
    newest_published_at: Option<i64>,
) -> bool {
    let fingerprint = build_source_fingerprint(total_results, first_page_ids);
    state.source_fingerprint.as_deref() == Some(fingerprint.as_str())
        && state.newest_published_at == newest_published_at
}

/// The page shape the conversion's pagination requests for its first page:
/// as much of the configured item budget as one API page can hold. This is
/// the only place that shape is derived — the pagination requests it and
/// records it next to the snapshot, and the Freshness Probe re-fetches
/// exactly what was recorded. Pure function so tests can exercise it.
///
/// ceiling: with a zero item budget this derives a probe page of 0 — a
/// degenerate config whose upstream behavior is not special-cased anywhere
/// in this module. Mirroring the pagination exactly is the invariant that
/// matters, and it outranks the degenerate config.
fn first_page_size(max_fetched_items: usize) -> u64 {
    max_fetched_items.min(50) as u64
}

/// The state a Freshness Probe observes with a single playlistItems.list call.
struct ProbePage {
    total_results: Option<i32>,
    first_page_ids: Vec<String>,
    newest_published_at: Option<i64>,
}

/// One ~1 quota unit call: the playlist's first page in position order plus
/// the playlist's total item count. Everything a probe needs to compare
/// against the state captured at Feed Conversion time.
///
/// The page size is not derived here: it is the shape the conversion's
/// pagination recorded next to its snapshot (`FeedProbeState::probe_page_size`),
/// so a probe's fingerprint is always derived from a page of the same shape
/// the conversion captured.
async fn fetch_probe_page(
    playlist_id: &str,
    page_size: u64,
    api_key: &str,
) -> eyre::Result<ProbePage> {
    let hub = get_youtube_hub();
    let response = hub
        .playlist_items()
        .list(&vec!["snippet".into()])
        .playlist_id(playlist_id)
        .param("key", api_key)
        .max_results(page_size.try_into()?)
        .doit()
        .await?;

    let items = response.1.items.unwrap_or_default();
    let (first_page_ids, newest_published_at) = first_page_snapshot(&items);

    Ok(ProbePage {
        total_results: response.1.page_info.and_then(|p| p.total_results),
        first_page_ids,
        newest_published_at,
    })
}

/// Canonical Feed Key identity for a youtube source url. Playlist and channel
/// urls are read straight from the url; handle and legacy urls are resolved to
/// their channel id through the same (redis-cached) yt-dlp resolution the feed
/// conversion uses, so no extra work is done on the happy path. Returns `None`
/// for urls this provider cannot canonically identify.
async fn canonical_yt_feed_id(url: &Url) -> Option<String> {
    let path = url.path();
    if path.starts_with("/playlist") {
        let list = url
            .query_pairs()
            .find(|(key, _)| key == "list")
            .map(|(_, value)| value.to_string())?;
        if list.is_empty() {
            return None;
        }
        return Some(format!("yt:playlist:{list}"));
    }
    if let Some(rest) = path.strip_prefix("/channel/") {
        let id = rest.trim_end_matches('/');
        if id.is_empty() {
            return None;
        }
        return Some(format!("yt:channel:{id}"));
    }
    if path.starts_with("/@") || path.starts_with("/user/") || path.starts_with("/c/") {
        let resolved = find_yt_channel_url_with_c_id(url).await.ok()?;
        let id = resolved.path_segments()?.next_back()?;
        if id.is_empty() {
            return None;
        }
        return Some(format!("yt:channel:{id}"));
    }
    None
}

/// Feed generation that never touches the metered YouTube Data API: the
/// provider's own public atom feed (limited to the latest items) with
/// durations resolved through yt-dlp. Used both when no API key is configured
/// and as the quota-free rung of the Degradation Ladder.
async fn generate_rss_feed_via_atom(channel_url: Url) -> eyre::Result<String> {
    let feed = match channel_url.path() {
        path if path.starts_with("/playlist") => feed_url_for_yt_playlist(&channel_url).await,
        path if path.starts_with("/feeds/") => feed_url_for_yt_atom(&channel_url).await,
        path if path.starts_with("/channel/") => feed_url_for_yt_channel(&channel_url).await,
        path if path.starts_with("/user/") => feed_url_for_yt_channel(&channel_url).await,
        path if path.starts_with("/c/") => feed_url_for_yt_channel(&channel_url).await,
        path if path.starts_with("/@") => feed_url_for_yt_channel(&channel_url).await,
        _ => Err(eyre!("unsupported youtube url")),
    }?;
    let response = reqwest::get(feed).await?;
    if !response.status().is_success() {
        return Err(eyre!(
            "YouTube feed returned error: {} {}",
            response.status().as_u16(),
            response.status().canonical_reason().unwrap_or("Unknown")
        ));
    }
    let raw_atom_feed = response.text().await?;
    let feed = feed_rs::parser::parse(&raw_atom_feed.into_bytes()[..])
        .map_err(|e| eyre!("Failed to parse YouTube feed: {}", e))?;
    let mut duration_map: HashMap<String, Option<usize>> = HashMap::default();
    let urls: Vec<String> = feed
        .entries
        .iter()
        .filter_map(|e| e.links.first())
        .map(|link| link.href.clone())
        .collect();

    let futures = urls.into_iter().map(|href| async move {
        let url = href.parse::<Url>()?;
        let duration = get_youtube_video_duration_with_ytdlp(&url).await?;
        Ok::<_, eyre::Error>((href, duration))
    });

    let results: Vec<_> = stream::iter(futures).buffer_unordered(4).collect().await;

    for result in results {
        let (href, duration) = result?;
        duration_map.insert(href, duration);
    }
    let filter_shorts = conf()
        .get(ConfName::YoutubeFilterShorts)
        .unwrap_or_else(|_| "false".to_string())
        .eq_ignore_ascii_case("true");
    Ok(convert_atom_to_rss(feed, duration_map, filter_shorts))
}

/// Everything a full YouTube API Feed Conversion produces: the rss channel +
/// items, the Freshness Probe state captured while fetching, and the number of
/// API requests spent (each costs ~1 quota unit).
struct ApiFetch {
    channel: Channel,
    items: Vec<Item>,
    probe_state: provider::FeedProbeState,
    api_requests: usize,
}

async fn fetch_from_api(
    id: IdType,
    api_key: String,
    filter_shorts: bool,
) -> eyre::Result<ApiFetch> {
    let (rss_channel, probe_target, upload_playlist) = match id {
        IdType::Playlist(id) => {
            info!("fetching playlist {}", id);
            let mut playlist = fetch_playlist(id, &api_key).await?;

            let playlist_id = playlist.id.take().ok_or(eyre!("playlist has no id"))?;

            let rss_channel = build_channel_from_playlist(playlist);
            (rss_channel, playlist_id.clone(), playlist_id)
        }
        IdType::Channel(id) => {
            info!("fetching channel {}", id);
            let mut channel = fetch_channel(id, &api_key).await?;

            let upload_playlist = channel
                .content_details
                .take()
                .ok_or(eyre!("content_details is None"))?
                .related_playlists
                .ok_or(eyre!("related_playlists is None"))?
                .uploads
                .ok_or(eyre!("uploads is None"))?;

            let rss_channel = build_channel_from_yt_channel(channel);
            (rss_channel, upload_playlist.clone(), upload_playlist)
        }
    };

    let max_fetched_items: usize = conf().get(ConfName::YoutubeMaxResults).unwrap().parse()?;
    let fetched = fetch_playlist_items(&upload_playlist, &api_key, max_fetched_items).await?;

    let probe_state = probe_state_from_fetch(&fetched, &probe_target);

    let (duration_map, video_batches) = create_duration_url_map(&fetched.items, &api_key).await?;

    let rss_items = build_channel_items_from_playlist(fetched.items, duration_map, filter_shorts);

    // the api requests spent: 1 for the playlist/channel detail call, plus the
    // playlist-items pages, plus the video-info batches
    let api_requests = 1 + fetched.api_requests + video_batches;
    info!(
        "quota: ~{} units consumed for full feed conversion of playlist {}",
        api_requests, probe_target
    );

    Ok(ApiFetch {
        channel: rss_channel,
        items: rss_items,
        probe_state,
        api_requests,
    })
}

macro_rules! get_thumb {
    ($snippet:ident) => {
        $snippet.thumbnails.and_then(|thumbs| {
            thumbs
                .maxres
                .or(thumbs.high)
                .or(thumbs.medium)
                .or(thumbs.standard)
                .or(thumbs.default)
        })
    };
}

fn build_channel_from_yt_channel(channel: api::Channel) -> Channel {
    let mut channel_builder = ChannelBuilder::default();
    let mut itunes_channel_builder = ITunesChannelExtensionBuilder::default();

    if let Some(mut snippet) = channel.snippet {
        channel_builder.description(snippet.description.take().unwrap_or("".to_owned()));
        channel_builder.title(snippet.title.take().unwrap_or("".to_owned()));
        channel_builder.language(snippet.default_language.take());
        if let Some(mut thumb) = get_thumb!(snippet) {
            itunes_channel_builder.image(thumb.url.take());
        }
    }
    provider::apply_apple_channel_tags(&mut itunes_channel_builder);
    channel_builder.link(format!(
        "https://www.youtube.com/channel/{}",
        channel.id.unwrap_or_default()
    ));

    channel_builder.itunes_ext(Some(itunes_channel_builder.build()));
    channel_builder.build()
}

async fn fetch_channel(id: String, api_key: &str) -> eyre::Result<api::Channel> {
    let hub = get_youtube_hub();
    let channel_request = hub
        .channels()
        .list(&vec!["snippet".into(), "contentDetails".into()])
        .max_results(1)
        .add_id(&id)
        .param("key", api_key);
    let result = channel_request.doit().await?;
    let channel = result
        .1
        .items
        .ok_or(eyre!("youtube returned no channel with id {:?}", id))?
        .first()
        .ok_or(eyre!("youtube returned no channel with id {:?}", id))?
        .clone();
    Ok(channel)
}

#[derive(Debug, Clone)]
struct VideoExtraInfo {
    duration: iso8601_duration::Duration,
}

async fn create_duration_url_map(
    items: &[PlaylistItem],
    api_key: &str,
) -> Result<(HashMap<String, VideoExtraInfo>, usize), eyre::Error> {
    let ids_batches = items.chunks(50).map(|c| {
        c.iter()
            .filter_map(|i| i.snippet.clone()?.resource_id?.video_id)
    });

    let hub = get_youtube_hub();
    let videos_requests = ids_batches.map(|batch| {
        let mut video_info_request = hub
            .videos()
            .list(&vec!["contentDetails".to_owned()])
            .param("key", api_key);

        for video_id in batch {
            video_info_request = video_info_request.add_id(&video_id);
        }
        video_info_request.doit()
    });

    info!(
        "fetching video info for {} videos in {} batches",
        items.len(),
        videos_requests.len()
    );

    let video_infos = futures::future::join_all(videos_requests)
        .await
        .into_iter()
        .flatten()
        .map(|r| {
            r.0.status()
                .is_success()
                .then(|| r.1.items.unwrap())
                .ok_or_else(|| eyre!("error fetching video info {:?}", r.0))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .filter_map(|v| {
            Some((
                v.id?,
                VideoExtraInfo {
                    duration: iso8601_duration::Duration::parse(&v.content_details?.duration?)
                        .unwrap(),
                },
            ))
        })
        .collect::<HashMap<_, _>>();

    // every videos.list batch costs ~1 quota unit
    let batches = items.len().div_ceil(50);
    Ok((video_infos, batches))
}

/// The per-Episode facts one rung-producing path extracts from its wire
/// format, ready for the shared shaper. Extraction cannot be shared (the
/// wire types differ); everything downstream of this struct is.
struct EpisodeFacts {
    title: Option<String>,
    description: Option<String>,
    guid: String,
    link: Option<String>,
    published_at: Option<DateTime<Utc>>,
    duration_seconds: Option<u64>,
    image_url: Option<String>,
    author: Option<String>,
}

/// The two rung-producing sources of Episodes. They differ in what an
/// unknown duration means and in how much their wire format guarantees;
/// the shaper preserves those differences instead of unifying them
/// blindly — a divergence here is where the cross-rung guid contract
/// (docs/adr/0002) would silently break.
#[derive(Clone, Copy)]
enum EpisodeSource {
    /// The metered YouTube Data API path. Durations are batch-fetched up
    /// front, so an episode without one cannot be shaped; title and
    /// description are always carried (empty string when the API omits
    /// them) and the author doubles on the plain <author> element and
    /// itunes:summary.
    Api,
    /// The quota-free atom path (the Degraded Feed rung). Durations come
    /// from best-effort yt-dlp lookups; every metadata field is optional
    /// and authoring is itunes-only.
    Atom,
}

/// The one shorts filter both rung-producing paths go through. A known
/// duration at or below the shorts threshold is filtered when shorts
/// filtering is on; what an unknown duration means is the source's call,
/// preserved exactly rather than unified:
/// - the API path drops the episode — its durations are batch-fetched, so
///   "unknown" means the upstream lookup failed,
/// - the atom path keeps the episode — its durations are best-effort
///   yt-dlp lookups, and a lookup failure must not shrink the feed.
fn shorts_filter_admits(
    duration_seconds: Option<u64>,
    filter_shorts: bool,
    source: EpisodeSource,
) -> bool {
    match duration_seconds {
        None => matches!(source, EpisodeSource::Atom),
        Some(seconds) => !(filter_shorts && seconds <= SHORTS_THRESHOLD_SECONDS),
    }
}

/// The one Episode shaper both rung-producing paths call. Owns every
/// per-Episode field a Feed item carries — the Episode Metadata (title,
/// description, artwork, publish date), the Episode Guid, duration, and
/// author — plus the shorts filter, so the two paths cannot drift apart
/// on any of them. Returns None when the shorts filter drops the episode.
fn shape_episode(facts: EpisodeFacts, source: EpisodeSource, filter_shorts: bool) -> Option<Item> {
    if !shorts_filter_admits(facts.duration_seconds, filter_shorts, source) {
        if let Some(seconds) = facts.duration_seconds {
            debug!(
                "filtering out short video: {} ({} seconds)",
                facts.guid, seconds
            );
        }
        return None;
    }

    let mut item_builder = ItemBuilder::default();
    item_builder.guid(Some(GuidBuilder::default().value(facts.guid).build()));
    item_builder.link(facts.link);
    item_builder.pub_date(facts.published_at.map(|date| date.to_rfc2822()));
    let mut itunes_item_builder = ITunesItemExtensionBuilder::default();
    itunes_item_builder.image(facts.image_url);
    itunes_item_builder.duration(facts.duration_seconds.map(format_duration));
    match source {
        // the API path guarantees title and description (empty string when
        // the API omits them) and doubles the author: Apple ignores plain
        // <author>, so episodes need itunes:author too
        EpisodeSource::Api => {
            item_builder.title(Some(facts.title.unwrap_or_default()));
            let description = Some(facts.description.unwrap_or_default());
            item_builder.description(description.clone());
            itunes_item_builder.summary(description);
            item_builder.author(facts.author.clone());
            itunes_item_builder.author(facts.author);
        }
        // the atom path carries only optional metadata and authors
        // itunes-only
        EpisodeSource::Atom => {
            item_builder.title(facts.title);
            item_builder.description(facts.description);
            itunes_item_builder.author(facts.author);
        }
    }
    item_builder.itunes_ext(Some(itunes_item_builder.build()));
    Some(item_builder.build())
}

/// Extracts one Episode's facts from a playlist item, dropping the items the
/// API path cannot shape (no snippet, no video id, unparsable guid) and
/// warning on the durations the API lookup failed to produce — the shaper's
/// shorts policy drops those, per the API path's unknown-duration semantics.
fn episode_facts_from_playlist_item(
    mut item: PlaylistItem,
    videos_infos: &HashMap<String, VideoExtraInfo>,
) -> Option<EpisodeFacts> {
    let mut snippet = item.snippet.take()?;
    let video_id = snippet.resource_id.take()?.video_id?;
    let url = Url::parse(&episode_identity::youtube_episode_guid(&video_id)).ok()?;

    let duration_seconds = match videos_infos.get(&video_id) {
        Some(info) => Some(
            (info.duration.hour * 3600.0 + info.duration.minute * 60.0 + info.duration.second)
                as u64,
        ),
        None => {
            warn!("no duration found for {:?}", video_id);
            None
        }
    };

    Some(EpisodeFacts {
        title: snippet.title.take(),
        description: snippet.description.take(),
        guid: url.to_string(),
        link: Some(url.to_string()),
        published_at: snippet.published_at,
        duration_seconds,
        image_url: get_thumb!(snippet).and_then(|t| t.url),
        author: snippet.channel_title.take(),
    })
}

/// Extracts one Episode's facts from an atom entry. Durations are a
/// best-effort lookup by watch URL — an unknown one keeps the episode (the
/// shaper's shorts policy preserves that). The channel title doubles as the
/// itunes:author; an empty one means no author.
fn episode_facts_from_atom_entry(
    entry: feed_rs::model::Entry,
    duration_map: &HashMap<String, Option<usize>>,
    channel_title: &str,
) -> EpisodeFacts {
    let link = entry.links.first().map(|d| d.clone().href);
    let duration_seconds = link
        .as_ref()
        .and_then(|link| duration_map.get(link))
        .and_then(|seconds| *seconds)
        .map(|seconds| seconds as u64);
    let media = entry.media.first();
    let description = media
        .and_then(|media| media.description.clone())
        .map(|description| description.content);
    let image_url = media
        .and_then(|media| media.thumbnails.first())
        .map(|thumbnail| thumbnail.image.uri.clone());

    EpisodeFacts {
        title: entry.title.map(|title| title.content),
        description,
        guid: episode_identity::youtube_episode_guid_from_atom(entry.id.as_str(), link.as_deref()),
        link,
        published_at: entry.published.or(entry.updated),
        duration_seconds,
        image_url,
        author: (!channel_title.is_empty()).then(|| channel_title.to_string()),
    }
}

fn build_channel_items_from_playlist(
    items: Vec<PlaylistItem>,
    videos_infos: HashMap<String, VideoExtraInfo>,
    filter_shorts: bool,
) -> Vec<Item> {
    items
        .into_iter()
        .filter_map(|item| episode_facts_from_playlist_item(item, &videos_infos))
        .filter_map(|facts| shape_episode(facts, EpisodeSource::Api, filter_shorts))
        .collect()
}

/// Result of fetching a playlist's items: the items sorted by publication
/// date, plus the raw source state needed to build a Freshness Probe state.
///
/// `total_results`, `first_page_ids` and `newest_published_at` are all derived
/// from the first page *in playlist position order* (before any sorting or
/// short filtering), and `first_page_size` is the maxResults that page was
/// requested with, so a probe re-fetching the recorded shape re-derives them
/// the exact same way and compares like with like.
struct PlaylistFetch {
    items: Vec<PlaylistItem>,
    /// pageInfo.totalResults of the first page: total items in the playlist
    /// as reported by the API.
    total_results: Option<i32>,
    /// Video ids of the first page, in playlist position order (up to 50).
    first_page_ids: Vec<String>,
    /// Newest publication timestamp (unix seconds) within the first page.
    newest_published_at: Option<i64>,
    /// The maxResults the first page was requested with — the page shape the
    /// snapshot fields above were captured from, recorded so the Freshness
    /// Probe re-fetches exactly it instead of re-deriving it.
    first_page_size: u64,
    /// How many playlistItems.list requests this fetch made (~1 quota unit each).
    api_requests: usize,
}

async fn fetch_playlist_items(
    playlist_id: &String,
    api_key: &str,
    max_fetched_items: usize,
) -> eyre::Result<PlaylistFetch> {
    let hub = get_youtube_hub();
    let max_consecutive_requests = (max_fetched_items / 50) + 1;
    let mut fetched_playlist_items: Vec<PlaylistItem> = Vec::with_capacity(max_fetched_items);
    let mut request_count = 0;
    let mut next_page_token: Option<String> = None;
    let mut total_results: Option<i32> = None;
    let mut first_page_ids: Vec<String> = Vec::new();
    let mut newest_published_at: Option<i64> = None;
    // derived once: page 0 is requested at this shape and the shape is
    // recorded with its snapshot for the Freshness Probe to re-fetch
    let first_page_size = first_page_size(max_fetched_items);
    debug!("fetching items from playlist {}", playlist_id);
    loop {
        let items_to_fetch = if request_count == 0 {
            first_page_size as usize
        } else {
            let remaining_items = max_fetched_items - fetched_playlist_items.len();
            if remaining_items > 50 {
                50
            } else {
                remaining_items
            }
        };

        let mut playlist_items_request = hub
            .playlist_items()
            .list(&vec!["snippet".into()])
            .playlist_id(playlist_id)
            .param("key", api_key)
            .max_results(items_to_fetch.try_into()?);

        if let Some(ref next_page_token) = next_page_token {
            playlist_items_request = playlist_items_request.page_token(next_page_token.as_str());
        }

        let response = playlist_items_request.doit().await?;

        let page_items = response
            .1
            .items
            .ok_or(eyre!("playlist object has no items field"))?;

        if request_count == 0 {
            total_results = response.1.page_info.and_then(|p| p.total_results);
            (first_page_ids, newest_published_at) = first_page_snapshot(&page_items);
        }

        fetched_playlist_items.extend(page_items);
        next_page_token = response.1.next_page_token;

        if next_page_token.is_none() || request_count == max_consecutive_requests {
            info!(
                "fetched {} items, max items reached or no more items to fetch",
                fetched_playlist_items.len()
            );
            break;
        }
        request_count += 1;
    }
    info!(
        "fetched {} items, in {} requests",
        fetched_playlist_items.len(),
        request_count + 1
    );
    fetched_playlist_items.sort_by_key(|i| i.snippet.as_ref().and_then(|s| s.published_at));
    Ok(PlaylistFetch {
        items: fetched_playlist_items,
        total_results,
        first_page_ids,
        newest_published_at,
        first_page_size,
        // +1 because request_count counts the pagination steps after the first call
        api_requests: request_count + 1,
    })
}

fn build_channel_from_playlist(playlist: api::Playlist) -> Channel {
    let mut channel_builder = ChannelBuilder::default();
    let mut itunes_channel_builder = ITunesChannelExtensionBuilder::default();

    if let Some(mut snippet) = playlist.snippet {
        channel_builder.description(snippet.description.take().unwrap_or("".to_owned()));
        channel_builder.title(snippet.title.take().unwrap_or("".to_owned()));
        channel_builder.language(snippet.default_language.take());
        channel_builder.link(format!(
            "https://www.youtube.com/playlist?list={}",
            playlist.id.unwrap_or_default()
        ));
        if let Some(mut thumb) = get_thumb!(snippet) {
            itunes_channel_builder.image(thumb.url.take());
        }
    }
    provider::apply_apple_channel_tags(&mut itunes_channel_builder);

    channel_builder.itunes_ext(Some(itunes_channel_builder.build()));
    channel_builder.build()
}

async fn fetch_playlist(id: String, api_key: &str) -> Result<api::Playlist, eyre::Error> {
    let hub = get_youtube_hub();
    let playlist_request = hub
        .playlists()
        .list(&vec!["snippet".into()])
        .add_id(&id)
        .param("key", api_key);
    let result = playlist_request.doit().await?;
    let playlist = result
        .1
        .items
        .ok_or(eyre!("youtube returned no playlist with id {:?}", id))?
        .first()
        .ok_or(eyre!("youtube returned no playlist with id {:?}", id))?
        .clone();
    Ok(playlist)
}

fn get_youtube_hub(
) -> YouTube<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>> {
    let auth = NoToken;
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("Failed to load native root certificates")
        .https_only()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new()).build(connector);

    YouTube::new(client, auth)
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

async fn feed_url_for_yt_playlist(url: &Url) -> eyre::Result<Url> {
    let playlist_id = url
        .query_pairs()
        .find(|(key, _)| key == "list")
        .map(|(_, value)| value)
        .ok_or_else(|| eyre::eyre!("Failed to parse playlist ID from URL: {}", url))?;

    let mut feed_url = Url::parse("https://www.youtube.com/feeds/videos.xml").unwrap();
    feed_url
        .query_pairs_mut()
        .append_pair("playlist_id", &playlist_id);

    Ok(feed_url)
}
async fn feed_url_for_yt_atom(url: &Url) -> eyre::Result<Url> {
    Ok(url.clone())
}

async fn feed_url_for_yt_channel(url: &Url) -> eyre::Result<Url> {
    info!("trying to convert youtube channel url {}", url);
    if url.to_string().contains("feeds/videos.xml") {
        return Ok(url.to_owned());
    }
    let url_with_channel_id = find_yt_channel_url_with_c_id(url).await?;
    let channel_id = url_with_channel_id
        .path_segments()
        .unwrap()
        .next_back()
        .unwrap();
    let mut feed_url = Url::parse("https://www.youtube.com/feeds/videos.xml")?;
    feed_url
        .query_pairs_mut()
        .append_pair("channel_id", channel_id);
    info!("converted to {feed_url}");
    Ok(feed_url)
}

#[cfg_attr(
    not(test),
    concurrent_cached(
        map_error = r##"|e| eyre::Error::new(e)"##,
        ty = "AsyncRedisCache<Url, Url>",
        create = r##" {
        AsyncRedisCache::builder("youtube_channel_username_to_id=")
            .ttl(std::time::Duration::from_secs(9999999))
            .refresh_on_hit(false)
            // Auto-reconnect, same reason as the stream-url cache: a dead
            // singleton connection must heal, not require an app restart.
            .connection_manager(true)
            .connection_string(&conf().get(ConfName::RedisUrl).unwrap())
            .build()
            .await
            .expect("youtube_channel_username_to_id cache")
} "##
    )
)]
async fn find_yt_channel_url_with_c_id(url: &Url) -> eyre::Result<Url> {
    info!("conversion not in cache, using yt-dlp for conversion...");
    let output = yt_dlp::run(&[
        "--playlist-items",
        "0",
        "-O",
        "playlist:channel_url",
        url.as_str(),
    ])
    .await
    .map_err(eyre::Error::new)?;
    let conversion = std::str::from_utf8(&output.stdout);
    let feed_url = match conversion {
        Ok(feed_url) => feed_url,
        Err(e) => {
            warn!(
                        "error while translating channel name using yt-dlp:\nerror: {}\nyt-dlp stdout: {}\nyt-dlp stderr: {}",
                        e,
                        conversion.unwrap_or_default(),
                        std::str::from_utf8(&output.stderr).unwrap_or_default()
                    );
            return Err(eyre::eyre!(e));
        }
    };
    Ok(Url::parse(feed_url)?)
}

fn convert_atom_to_rss(
    feed: Feed,
    duration_map: HashMap<String, Option<usize>>,
    filter_shorts: bool,
) -> String {
    let mut feed_builder = provider::build_default_rss_structure();
    let channel_title = feed.title.clone().map(|d| d.content).unwrap_or_default();
    feed_builder.description(feed.description.map(|d| d.content).unwrap_or_default());
    feed_builder.title(channel_title.clone());
    feed_builder.language(feed.language);
    let mut image_builder = ImageBuilder::default();
    image_builder.url(feed.icon.clone().map(|d| d.uri).unwrap_or_default());
    feed_builder.image(Some(image_builder.build()));
    feed_builder.link(
        feed.links
            .clone()
            .first()
            .map(|d| d.clone().href)
            .unwrap_or_default(),
    );
    let mut itunes_ext_builder = ITunesChannelExtensionBuilder::default();
    itunes_ext_builder.image(feed.icon.map(|d| d.uri));
    provider::apply_apple_channel_tags(&mut itunes_ext_builder);
    feed_builder.itunes_ext(Some(itunes_ext_builder.build()));
    let items = feed
        .entries
        .into_iter()
        .map(|entry| episode_facts_from_atom_entry(entry, &duration_map, &channel_title))
        .filter_map(|facts| shape_episode(facts, EpisodeSource::Atom, filter_shorts))
        .collect::<Vec<Item>>();
    feed_builder.items(items);
    feed_builder.build().to_string()
}

#[cfg_attr(
    not(test),
    concurrent_cached(
        map_error = r##"|e| eyre::Error::new(e)"##,
        ty = "AsyncRedisCache<Url, Option<usize>>",
        create = r##" {
        AsyncRedisCache::builder("cached_yt_video_duration=")
            .ttl(std::time::Duration::from_secs(86400))
            .refresh_on_hit(false)
            // Auto-reconnect, same reason as the stream-url cache: a dead
            // singleton connection must heal, not require an app restart.
            .connection_manager(true)
            .connection_string(&conf().get(ConfName::RedisUrl).unwrap())
            .build()
            .await
            .expect("youtube_duration cache")
} "##
    )
)]
async fn get_youtube_video_duration_with_ytdlp(url: &Url) -> eyre::Result<Option<usize>> {
    debug!("getting duration for yt video: {}", url);

    let output = match yt_dlp::run(&["--get-duration", url.as_str()]).await {
        Ok(output) => output,
        Err(yt_dlp::YtDlpError::Timeout) => {
            warn!(
                "yt-dlp duration fetch timed out after {}s",
                yt_dlp::TIMEOUT.as_secs()
            );
            return Ok(None);
        }
        Err(yt_dlp::YtDlpError::Spawn(_)) => {
            warn!("could not parse youtube video duration");
            return Ok(None);
        }
    };

    let duration_str = std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .to_string();
    Ok(Some(
        parse_duration(&duration_str)
            .unwrap_or_default()
            .as_secs()
            .try_into()
            .unwrap(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_log::test;

    #[tokio::test]
    async fn test_build_items_for_playlist_requires_api_key() {
        let id = "UUXuqSBlHAE6Xw-yeJA0Tunw".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        println!("{:?}", playlist.clone().id.unwrap().clone());
        let fetched = fetch_playlist_items(&playlist.id.unwrap(), &api_key, 300)
            .await
            .unwrap();

        println!("{:?}", fetched.items);
        assert!(!fetched.items.is_empty())
    }

    #[tokio::test]
    async fn test_less_than_50_items_requires_api_key() {
        let id = "UUXuqSBlHAE6Xw-yeJA0Tunw".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        println!("{:?}", playlist.clone().id.unwrap().clone());
        let fetched = fetch_playlist_items(&playlist.id.unwrap(), &api_key, 13)
            .await
            .unwrap();

        println!("{:?}", fetched.items);
        assert!(!fetched.items.is_empty());
        assert_eq!(fetched.items.len(), 13)
    }

    #[tokio::test]
    async fn test_less_than_300_items_requires_api_key() {
        let id = "UUXuqSBlHAE6Xw-yeJA0Tunw".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        println!("{:?}", playlist.clone().id.unwrap().clone());
        let fetched = fetch_playlist_items(&playlist.id.unwrap(), &api_key, 50)
            .await
            .unwrap();

        println!("{:?}", fetched.items);
        assert!(!fetched.items.is_empty());
        assert_eq!(fetched.items.len(), 50)
    }

    #[tokio::test]
    async fn test_more_than_300_items_requires_api_key() {
        let id = "UUXuqSBlHAE6Xw-yeJA0Tunw".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        println!("{:?}", playlist.clone().id.unwrap().clone());
        let fetched = fetch_playlist_items(&playlist.id.unwrap(), &api_key, 600)
            .await
            .unwrap();

        println!("{:?}", fetched.items);
        assert!(!fetched.items.is_empty());
        assert_eq!(fetched.items.len(), 600)
    }

    #[test(tokio::test)]
    async fn test_build_channel_for_playlist_requires_api_key() {
        let id = "PLJmimp-uZX42T7ONp1FLXQDJrRxZ-_1Ct".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        let channel = build_channel_from_playlist(playlist);

        println!("{:?}", channel);
        assert!(!channel.description.is_empty());
        assert!(!channel.title.is_empty());
        assert!(channel.itunes_ext.unwrap().image.is_some());
    }

    #[test(tokio::test)]
    async fn test_fetch_playlist_requires_api_key() {
        let id = "PLJmimp-uZX42T7ONp1FLXQDJrRxZ-_1Ct".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let result = fetch_playlist(id, &api_key).await;

        println!("{:?}", result);
        assert!(result.is_ok());

        if let Ok(playlist) = result {
            assert!(playlist.id.is_some());
            assert!(playlist.snippet.is_some());
        }
    }

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

    #[test]
    fn test_probe_state_unchanged_when_source_matches() {
        let state = provider::FeedProbeState {
            newest_published_at: Some(1_700_000_000),
            source_fingerprint: Some(build_source_fingerprint(
                Some(120),
                &["vid1".to_string(), "vid2".to_string()],
            )),
            probe_target: Some("PLxyz".to_string()),
            probe_page_size: Some(50),
        };

        assert!(probe_state_unchanged(
            &state,
            Some(120),
            &["vid1".to_string(), "vid2".to_string()],
            Some(1_700_000_000)
        ));
    }

    #[test]
    fn test_probe_state_changed_when_source_changes() {
        let state = provider::FeedProbeState {
            newest_published_at: Some(1_700_000_000),
            source_fingerprint: Some(build_source_fingerprint(
                Some(120),
                &["vid1".to_string(), "vid2".to_string()],
            )),
            probe_target: Some("PLxyz".to_string()),
            probe_page_size: Some(50),
        };

        // a new video appended at the end of the playlist
        assert!(!probe_state_unchanged(
            &state,
            Some(121),
            &["vid1".to_string(), "vid2".to_string()],
            Some(1_700_000_000)
        ));
        // an item swapped inside the first page
        assert!(!probe_state_unchanged(
            &state,
            Some(120),
            &["vid1".to_string(), "vid3".to_string()],
            Some(1_700_000_000)
        ));
        // a newer publication inside the first page
        assert!(!probe_state_unchanged(
            &state,
            Some(120),
            &["vid1".to_string(), "vid2".to_string()],
            Some(1_800_000_000)
        ));
        // state was never captured
        let empty_state = provider::FeedProbeState::default();
        assert!(!probe_state_unchanged(
            &empty_state,
            Some(120),
            &["vid1".to_string()],
            Some(1_700_000_000)
        ));
    }

    #[test]
    fn test_probe_state_without_recorded_page_size_still_loads() {
        // cache entries stored before the page shape was recorded must still
        // deserialize; their probe cannot fetch a trusted shape and falls
        // through to a full conversion, which re-captures it
        let state: provider::FeedProbeState = serde_json::from_str(
            r#"{"newest_published_at":1700000000,"source_fingerprint":"120|vid1","probe_target":"PLxyz"}"#,
        )
        .unwrap();
        assert_eq!(state.probe_page_size, None);
        assert_eq!(state.probe_target, Some("PLxyz".to_string()));
    }

    #[test]
    fn test_build_source_fingerprint_is_order_sensitive() {
        let a = build_source_fingerprint(Some(2), &["vid1".to_string(), "vid2".to_string()]);
        let b = build_source_fingerprint(Some(2), &["vid2".to_string(), "vid1".to_string()]);
        assert_ne!(a, b);
    }

    #[test]
    fn test_first_page_size_is_the_shape_probes_refetch() {
        // default config: full page
        assert_eq!(first_page_size(300), 50);
        // small YOUTUBE_MAX_RESULTS: the conversion's first page and the
        // probe's re-fetch are the same shape by construction
        assert_eq!(first_page_size(13), 13);
        assert_eq!(first_page_size(1), 1);
        assert_eq!(first_page_size(49), 49);
        assert_eq!(first_page_size(50), 50);
        // degenerate config values mirror the pagination exactly, leaving
        // the upstream's handling of it un-special-cased
        assert_eq!(first_page_size(0), 0);
    }

    #[test]
    fn test_probe_fetches_the_page_shape_the_conversion_captured() {
        // the conversion's pagination records the shape of the very page the
        // snapshot was captured from, as one record ...
        let fetched = PlaylistFetch {
            items: vec![],
            total_results: Some(120),
            first_page_ids: vec!["vid1".to_string(), "vid2".to_string()],
            newest_published_at: Some(1_700_000_000),
            // a shape the default-config derivation (300-item budget -> 50)
            // would not produce, so a re-derived value cannot masquerade as
            // the recorded one
            first_page_size: 13,
            api_requests: 1,
        };
        let state = probe_state_from_fetch(&fetched, "PLxyz");

        // ... and the page the probe fetches is that record, nothing else
        assert_eq!(state.probe_page_size, Some(13));

        // a page re-fetched at the recorded shape re-derives the snapshot and
        // answers "unchanged"; a page of any other shape does not match
        assert!(probe_state_unchanged(
            &state,
            fetched.total_results,
            &fetched.first_page_ids,
            fetched.newest_published_at,
        ));
        assert!(!probe_state_unchanged(
            &state,
            Some(120),
            &["vid1".to_string(), "vid2".to_string(), "vid3".to_string()],
            fetched.newest_published_at,
        ));
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

    #[test]
    fn test_first_page_snapshot_extracts_ids_and_newest() {
        let items: Vec<PlaylistItem> = vec![
            PlaylistItem {
                snippet: Some(api::PlaylistItemSnippet {
                    published_at: Some(
                        chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                            .unwrap()
                            .into(),
                    ),
                    resource_id: Some(api::ResourceId {
                        video_id: Some("vid1".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            PlaylistItem {
                snippet: Some(api::PlaylistItemSnippet {
                    published_at: Some(
                        chrono::DateTime::parse_from_rfc3339("2024-06-01T00:00:00Z")
                            .unwrap()
                            .into(),
                    ),
                    resource_id: Some(api::ResourceId {
                        video_id: Some("vid2".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            // an item without a video id is skipped
            PlaylistItem::default(),
        ];

        let (ids, newest) = first_page_snapshot(&items);
        assert_eq!(ids, vec!["vid1".to_string(), "vid2".to_string()]);
        assert_eq!(newest, Some(1_717_200_000));
    }

    #[tokio::test]
    async fn test_canonical_feed_id_for_playlist_url() {
        let url = Url::parse("https://www.youtube.com/playlist?list=PL589F357911E267F7").unwrap();
        assert_eq!(
            canonical_yt_feed_id(&url).await,
            Some("yt:playlist:PL589F357911E267F7".to_string())
        );
    }

    #[tokio::test]
    async fn test_canonical_feed_id_for_channel_url() {
        let url = Url::parse("https://www.youtube.com/channel/UCXssEBQ8JWH1NacVIyQXe8g").unwrap();
        assert_eq!(
            canonical_yt_feed_id(&url).await,
            Some("yt:channel:UCXssEBQ8JWH1NacVIyQXe8g".to_string())
        );
    }

    #[tokio::test]
    async fn test_canonical_feed_id_unsupported_urls_return_none() {
        // no list param
        let url = Url::parse("https://www.youtube.com/playlist").unwrap();
        assert_eq!(canonical_yt_feed_id(&url).await, None);
        // atom feed urls have no canonical id
        let url = Url::parse(
            "https://www.youtube.com/feeds/videos.xml?channel_id=UCXssEBQ8JWH1NacVIyQXe8g",
        )
        .unwrap();
        assert_eq!(canonical_yt_feed_id(&url).await, None);
    }

    // -- Episode identity: guids and pubDates must be identical across the API
    // path and the quota-free atom path, or podcatchers re-identify every
    // episode whenever the Degradation Ladder switches rungs (mass duplication).

    const ATOM_FEED_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:media="http://search.yahoo.com/mrss/" xmlns:yt="http://www.youtube.com/xml/schemas/2015">
  <title>Test Channel</title>
  <id>yt:channel:UC123</id>
  <updated>2024-01-02T03:04:05+00:00</updated>
  <link rel="alternate" href="https://www.youtube.com/channel/UC123"/>
  <icon>https://example.com/avatar.jpg</icon>
  <entry>
    <id>yt:video:abc123</id>
    <title>Video One</title>
    <published>2024-01-01T10:00:00+00:00</published>
    <updated>2024-01-01T10:05:00+00:00</updated>
    <link rel="alternate" href="https://www.youtube.com/watch?v=abc123"/>
    <media:thumbnail url="https://example.com/thumb1.jpg" width="1280" height="720"/>
  </entry>
  <entry>
    <id>yt:video:def456</id>
    <title>Video Two</title>
    <updated>2024-01-02T11:00:00+00:00</updated>
    <link rel="alternate" href="https://www.youtube.com/watch?v=def456"/>
    <media:thumbnail url="https://example.com/thumb2.jpg" width="1280" height="720"/>
  </entry>
</feed>"#;

    fn convert_fixture(atom: &str) -> rss::Channel {
        let feed = feed_rs::parser::parse(atom.as_bytes()).unwrap();
        let body = convert_atom_to_rss(feed, HashMap::new(), false);
        rss::Channel::read_from(body.as_bytes()).unwrap()
    }

    #[test]
    fn test_atom_items_use_watch_url_guids() {
        let channel = convert_fixture(ATOM_FEED_FIXTURE);
        let guids: Vec<String> = channel
            .items
            .iter()
            .map(|i| i.guid().unwrap().value().to_string())
            .collect();
        assert_eq!(
            guids,
            vec![
                "https://www.youtube.com/watch?v=abc123".to_string(),
                "https://www.youtube.com/watch?v=def456".to_string(),
            ]
        );
    }

    #[test]
    fn test_api_items_use_watch_url_guids() {
        let items: Vec<PlaylistItem> = vec![PlaylistItem {
            snippet: Some(api::PlaylistItemSnippet {
                title: Some("Video One".to_string()),
                resource_id: Some(api::ResourceId {
                    video_id: Some("abc123".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let infos: HashMap<String, VideoExtraInfo> = HashMap::from([(
            "abc123".to_string(),
            VideoExtraInfo {
                duration: iso8601_duration::Duration::parse("PT10M").unwrap(),
            },
        )]);
        let rss_items = build_channel_items_from_playlist(items, infos, false);
        assert_eq!(
            rss_items[0].guid().unwrap().value(),
            "https://www.youtube.com/watch?v=abc123"
        );
    }

    #[test]
    fn test_next_quota_reset_is_next_pacific_midnight() {
        // 2026-09-21T04:27Z is 2026-09-20 21:27 PDT (UTC-7): reset is
        // 2026-09-21 00:00 PDT == 07:00Z
        let now = DateTime::parse_from_rfc3339("2026-09-21T04:27:35Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-09-21T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(YoutubeProvider.next_quota_reset(now), Some(expected));
    }

    #[test]
    fn test_next_quota_reset_during_pst_winter() {
        // 2026-01-15T08:00Z is exactly midnight PST (UTC-8): reset is the
        // NEXT midnight, 2026-01-16 08:00Z
        let now = DateTime::parse_from_rfc3339("2026-01-15T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-01-16T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(YoutubeProvider.next_quota_reset(now), Some(expected));
    }

    #[test]
    fn test_next_quota_reset_day_before_dst_start() {
        // DST starts 2026-03-08 at 02:00; midnight of 2026-03-08 is still PST
        let now = DateTime::parse_from_rfc3339("2026-03-07T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-03-08T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(YoutubeProvider.next_quota_reset(now), Some(expected));
    }

    #[test]
    fn test_quota_usage_day_is_the_pacific_date() {
        // 2026-09-21T04:27Z is still 2026-09-20 in Pacific time
        let now = DateTime::parse_from_rfc3339("2026-09-21T04:27:35Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            YoutubeProvider.quota_usage_day(now),
            Some("2026-09-20".to_string())
        );
    }

    #[test]
    fn test_quota_breaker_key_is_provider_owned() {
        assert_eq!(
            YoutubeProvider.quota_breaker_key(),
            Some("vod2pod:quota_breaker:youtube".to_string())
        );
    }

    #[test]
    fn test_youtube_quota_error_maps_to_typed_exhaustion() {
        // the real body shape of a quota-exhausted YouTube Data API call
        let body = serde_json::json!({
            "error": {
                "code": 403,
                "errors": [
                    {"domain": "youtube.quota", "reason": "quotaExceeded"}
                ]
            }
        });
        let err = eyre::Report::from(google_youtube3::Error::BadRequest(body))
            .wrap_err("could not generate feed");

        let mapped = youtube_quota_typed(err);

        assert!(mapped.is::<provider::QuotaExhausted>());
        assert!(
            format!("{mapped:#}").contains("quotaExceeded"),
            "the upstream failure is preserved for diagnosis"
        );
    }

    #[test]
    fn test_other_conversion_errors_pass_through_untouched() {
        let mapped = youtube_quota_typed(eyre::eyre!("playlist not found"));

        assert!(!mapped.is::<provider::QuotaExhausted>());
        assert_eq!(format!("{mapped:#}"), "playlist not found");
    }

    #[test]
    fn test_atom_items_get_pub_date_from_published() {
        let channel = convert_fixture(ATOM_FEED_FIXTURE);
        let expected = chrono::DateTime::parse_from_rfc3339("2024-01-01T10:00:00+00:00")
            .unwrap()
            .to_rfc2822()
            .to_string();
        assert_eq!(
            channel.items[0].pub_date().unwrap(),
            expected.as_str(),
            "pubDate must come from <published>"
        );
    }

    #[test]
    fn test_atom_item_pub_date_falls_back_to_updated() {
        let channel = convert_fixture(ATOM_FEED_FIXTURE);
        let expected = chrono::DateTime::parse_from_rfc3339("2024-01-02T11:00:00+00:00")
            .unwrap()
            .to_rfc2822()
            .to_string();
        // second entry has no <published>, only <updated>
        assert_eq!(
            channel.items[1].pub_date().unwrap(),
            expected.as_str(),
            "pubDate must fall back to <updated> when <published> is missing"
        );
    }

    #[test]
    fn test_atom_item_guid_falls_back_to_link_for_unknown_id_prefix() {
        let atom = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Test Channel</title>
  <id>urn:test</id>
  <updated>2024-01-02T03:04:05+00:00</updated>
  <entry>
    <id>some-other-scheme:xyz</id>
    <title>Video X</title>
    <published>2024-01-01T10:00:00+00:00</published>
    <link rel="alternate" href="https://www.youtube.com/watch?v=xyz789"/>
  </entry>
</feed>"#;
        let channel = convert_fixture(atom);
        assert_eq!(
            channel.items[0].guid().unwrap().value(),
            "https://www.youtube.com/watch?v=xyz789"
        );
    }

    #[test]
    fn test_atom_channel_carries_apple_conformance_tags() {
        let channel = convert_fixture(ATOM_FEED_FIXTURE);
        let itunes = channel.itunes_ext().unwrap();
        assert_eq!(
            itunes.explicit(),
            Some("false"),
            "Apple accepts only true/false"
        );
        assert_eq!(
            itunes.block(),
            Some("Yes"),
            "private feeds must stay out of the Apple directory"
        );
        assert_eq!(itunes.r#type(), Some("episodic"));
    }

    #[test]
    fn test_atom_items_carry_itunes_author() {
        let channel = convert_fixture(ATOM_FEED_FIXTURE);
        for item in &channel.items {
            assert_eq!(
                item.itunes_ext().and_then(|i| i.author()),
                Some("Test Channel"),
                "Apple ignores plain <author>; items need itunes:author"
            );
        }
    }

    // -- Episode Metadata: the media-derived fields a podcatcher renders
    // (description, artwork) must survive the quota-free atom path. Pinned
    // against a real captured feed because the synthetic fixture above cannot
    // express the media:group structure YouTube actually sends — exactly where
    // parser majors have broken us before. The file is a pinned contract
    // input: entries are verbatim, unselected ones removed. Re-capture with:
    //   curl -s "https://www.youtube.com/feeds/videos.xml?channel_id=UCBR8-60-B28hp2BmDPdntcQ"
    // (keyless public endpoint — no API key involved).

    const REAL_ATOM_FEED_FIXTURE: &str = include_str!("testdata/youtube_channel_atom.xml");

    #[test]
    fn test_real_atom_feed_yields_one_item_per_entry() {
        let channel = convert_fixture(REAL_ATOM_FEED_FIXTURE);
        assert_eq!(channel.items.len(), 3, "one Episode per entry");
        let titles: Vec<&str> = channel.items.iter().map(|i| i.title().unwrap()).collect();
        assert_eq!(
            titles,
            vec![
                "*cries in micro bang*",
                "20 years of esports history explained",
                "one year married and 5 full-length episodes, exclusively on YouTube on @DorothyWang's channel 💍",
            ]
        );
    }

    #[test]
    fn test_real_atom_item_description_survives_empty_media_description() {
        let channel = convert_fixture(REAL_ATOM_FEED_FIXTURE);
        // Real YouTube feeds routinely emit empty <media:description>; it must
        // not kill the parse nor produce phantom text. (feed-rs yields Some("")
        // here; the rss round-trip in convert_fixture normalizes the empty
        // element back to None, so the observable contract is absent-or-empty.)
        assert!(
            channel.items[0].description().is_none_or(|d| d.is_empty()),
            "empty media:description must stay an empty Episode description"
        );
        assert!(channel.items[2].description().is_none_or(|d| d.is_empty()));
        let long = channel.items[1]
            .description()
            .expect("populated description");
        assert!(
            long.starts_with("Join @Valkyrae"),
            "description text survives"
        );
        assert!(
            long.contains("Created & Produced by Portal A"),
            "XML entities must be decoded (&amp; → &)"
        );
        assert!(!long.contains("&amp;"), "no raw entity may leak through");
        assert!(
            long.ends_with("Sound Mix: Austin Krier"),
            "description must arrive complete, not silently truncated"
        );
    }

    #[test]
    fn test_real_atom_item_carries_media_thumbnail_as_itunes_image() {
        let channel = convert_fixture(REAL_ATOM_FEED_FIXTURE);
        // media:thumbnail lives inside media:group in real feeds; it must land
        // on the Episode as itunes:image or podcatchers lose all artwork.
        let images: Vec<Option<&str>> = channel
            .items
            .iter()
            .map(|i| i.itunes_ext().and_then(|it| it.image()))
            .collect();
        assert_eq!(
            images,
            vec![
                Some("https://i1.ytimg.com/vi/dWuPnW0EHKM/hqdefault.jpg"),
                Some("https://i4.ytimg.com/vi/71ya_JC_IbM/hqdefault.jpg"),
                Some("https://i2.ytimg.com/vi/q4-rVbyEmrc/hqdefault.jpg"),
            ]
        );
    }

    #[test]
    fn test_real_atom_items_carry_watch_url_guids_and_pub_dates() {
        let channel = convert_fixture(REAL_ATOM_FEED_FIXTURE);
        // Two of the three captured entries are Shorts; the Episode Guid scheme
        // is the watch URL regardless of the entry's link shape, or the API
        // path and this path would re-identify episodes (mass duplication).
        let guids: Vec<&str> = channel
            .items
            .iter()
            .map(|i| i.guid().unwrap().value())
            .collect();
        assert_eq!(
            guids,
            vec![
                "https://www.youtube.com/watch?v=dWuPnW0EHKM",
                "https://www.youtube.com/watch?v=71ya_JC_IbM",
                "https://www.youtube.com/watch?v=q4-rVbyEmrc",
            ]
        );
        for item in &channel.items {
            assert!(
                item.pub_date().is_some_and(|d| !d.is_empty()),
                "every Episode needs a pubDate"
            );
        }
    }

    // -- the one shorts filter ----------------------------------------------------
    //
    // Pins `shorts_filter_admits` and the one per-path difference it must
    // preserve: the API path drops episodes with unknown duration, the
    // atom path keeps them.

    #[test]
    fn test_shorts_filter_decision_table() {
        let short = Some(SHORTS_THRESHOLD_SECONDS); // at the threshold: a short
        let long = Some(SHORTS_THRESHOLD_SECONDS + 1);

        // known durations: the threshold applies on both paths alike
        for source in [EpisodeSource::Api, EpisodeSource::Atom] {
            assert!(
                !shorts_filter_admits(short, true, source),
                "a short is filtered"
            );
            assert!(
                shorts_filter_admits(long, true, source),
                "a long video survives"
            );
            assert!(
                shorts_filter_admits(short, false, source),
                "with shorts filtering off nothing is filtered"
            );
        }
        // an unknown duration means opposite things per path, whatever the
        // shorts setting: the API path drops, the atom path keeps
        assert!(!shorts_filter_admits(None, true, EpisodeSource::Api));
        assert!(!shorts_filter_admits(None, false, EpisodeSource::Api));
        assert!(shorts_filter_admits(None, true, EpisodeSource::Atom));
        assert!(shorts_filter_admits(None, false, EpisodeSource::Atom));
    }

    #[test]
    fn test_api_path_drops_episodes_with_unknown_duration() {
        // the API path batch-fetches durations; an episode the lookup failed
        // for is dropped even with shorts filtering off — unknown means
        // unshapable there
        let items: Vec<PlaylistItem> = vec![PlaylistItem {
            snippet: Some(api::PlaylistItemSnippet {
                title: Some("Video One".to_string()),
                resource_id: Some(api::ResourceId {
                    video_id: Some("abc123".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let rss_items = build_channel_items_from_playlist(items, HashMap::new(), false);
        assert!(
            rss_items.is_empty(),
            "an unknown duration must drop the episode on the API path"
        );
    }

    #[test]
    fn test_atom_path_keeps_episodes_with_unknown_duration() {
        // the atom path's durations are best-effort yt-dlp lookups; a lookup
        // failure must not shrink the feed. The real fixture carries two
        // Shorts, so with durations known and shorts filtering on every
        // episode is filtered — pinning the unknown-duration keep exactly.
        let feed = feed_rs::parser::parse(REAL_ATOM_FEED_FIXTURE.as_bytes()).unwrap();
        let body = convert_atom_to_rss(feed, HashMap::new(), true);
        let channel = rss::Channel::read_from(body.as_bytes()).unwrap();
        assert_eq!(
            channel.items.len(),
            3,
            "unknown durations must keep every episode on the atom path"
        );

        // the same feed with known short durations loses every episode
        let known_shorts: HashMap<String, Option<usize>> = [
            // the fixture's raw link shapes: two shorts-style, one watch URL
            "https://www.youtube.com/shorts/dWuPnW0EHKM",
            "https://www.youtube.com/watch?v=71ya_JC_IbM",
            "https://www.youtube.com/shorts/q4-rVbyEmrc",
        ]
        .into_iter()
        .map(|url| (url.to_string(), Some(60)))
        .collect();
        let feed = feed_rs::parser::parse(REAL_ATOM_FEED_FIXTURE.as_bytes()).unwrap();
        let body = convert_atom_to_rss(feed, known_shorts, true);
        let channel = rss::Channel::read_from(body.as_bytes()).unwrap();
        assert!(
            channel.items.is_empty(),
            "known short durations must filter every episode"
        );
    }

    #[test]
    fn test_api_item_carries_full_metadata() {
        // the API path has no golden test upstream of the shaper, so pin the
        // metadata the shaper owns: title, description, guid, artwork, publish
        // date, duration, and the doubled author
        let items: Vec<PlaylistItem> = vec![PlaylistItem {
            snippet: Some(api::PlaylistItemSnippet {
                title: Some("Video One".to_string()),
                description: Some("A description".to_string()),
                channel_title: Some("Test Channel".to_string()),
                published_at: Some(
                    chrono::DateTime::parse_from_rfc3339("2024-01-01T10:00:00Z")
                        .unwrap()
                        .into(),
                ),
                resource_id: Some(api::ResourceId {
                    video_id: Some("abc123".to_string()),
                    ..Default::default()
                }),
                thumbnails: Some(api::ThumbnailDetails {
                    high: Some(api::Thumbnail {
                        url: Some("https://example.com/thumb.jpg".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let infos: HashMap<String, VideoExtraInfo> = HashMap::from([(
            "abc123".to_string(),
            VideoExtraInfo {
                duration: iso8601_duration::Duration::parse("PT10M").unwrap(),
            },
        )]);
        let rss_items = build_channel_items_from_playlist(items, infos, false);
        assert_eq!(rss_items.len(), 1);
        let item = &rss_items[0];
        assert_eq!(item.title.as_deref(), Some("Video One"));
        assert_eq!(item.description.as_deref(), Some("A description"));
        assert_eq!(
            item.guid().unwrap().value(),
            "https://www.youtube.com/watch?v=abc123"
        );
        assert_eq!(item.author.as_deref(), Some("Test Channel"));
        assert_eq!(
            item.pub_date().unwrap(),
            chrono::DateTime::parse_from_rfc3339("2024-01-01T10:00:00Z")
                .unwrap()
                .to_rfc2822()
        );
        let itunes = item.itunes_ext().unwrap();
        assert_eq!(itunes.author(), Some("Test Channel"));
        assert_eq!(itunes.summary(), Some("A description"));
        assert_eq!(itunes.duration(), Some("00:10:00"));
        assert_eq!(itunes.image(), Some("https://example.com/thumb.jpg"));
    }
}
