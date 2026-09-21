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
use std::{collections::HashMap, str::FromStr, time::Duration};

use async_trait::async_trait;
use eyre::eyre;
use futures::stream::{self, StreamExt};
use log::{debug, info, warn};
use regex::Regex;
use reqwest::Url;
use rss::{
    extension::itunes::{ITunesChannelExtensionBuilder, ITunesItemExtensionBuilder},
    Channel, ChannelBuilder, GuidBuilder, ImageBuilder, Item, ItemBuilder,
};
use tokio::process::Command;

use crate::{
    configs::{conf, Conf, ConfName},
    provider,
};

use super::{GeneratedFeed, MediaProvider};

pub struct YoutubeProvider;

/// Duration threshold for YouTube Shorts (180 seconds = 3 minutes)
const SHORTS_THRESHOLD_SECONDS: u64 = 180;

enum IdType {
    Playlist(String),
    Channel(String),
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

                let mut fetched = fetch_from_api(id, api_key, filter_shorts).await?;

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
        let api_key = conf().get(ConfName::YoutubeApiKey)?;
        let probe_target = state
            .probe_target
            .as_deref()
            .ok_or_else(|| eyre!("probe state has no probe target"))?;

        let page = fetch_probe_page(probe_target, &api_key).await?;
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
        Some(crate::feed_cache::YT_QUOTA_BREAKER_KEY.to_string())
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

/// Page size a probe must fetch so its fingerprint is derived from the same
/// page shape a full conversion captures first. Pure function so tests can
/// exercise it.
fn probe_page_size(max_fetched_items: usize) -> u64 {
    max_fetched_items.clamp(1, 50) as u64
}

/// The state a Freshness Probe observes with a single playlistItems.list call.
struct ProbePage {
    total_results: Option<i32>,
    first_page_ids: Vec<String>,
    newest_published_at: Option<i64>,
}

/// One ~1 quota unit call: first page of the playlist in position order plus
/// the playlist's total item count. Everything a probe needs to compare
/// against the state captured at Feed Conversion time.
///
/// The page size matches the one a full conversion would fetch first
/// (min(YOUTUBE_MAX_RESULTS, 50)), so the fingerprint and the newest
/// publication timestamp are derived from the same page shape the conversion
/// captured; a mismatch here would make every probe report "changed".
async fn fetch_probe_page(playlist_id: &str, api_key: &str) -> eyre::Result<ProbePage> {
    let max_fetched_items: usize = conf()
        .get(ConfName::YoutubeMaxResults)
        .unwrap_or_else(|_| "300".to_string())
        .parse()
        .unwrap_or(300);
    let page_size = probe_page_size(max_fetched_items);
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

    let (duration_map, video_batches) = create_duration_url_map(&fetched.items, &api_key).await?;

    let rss_items = build_channel_items_from_playlist(fetched.items, duration_map, filter_shorts);

    // the api requests spent: 1 for the playlist/channel detail call, plus the
    // playlist-items pages, plus the video-info batches
    let api_requests = 1 + fetched.api_requests + video_batches;
    info!(
        "quota: ~{} units consumed for full feed conversion of playlist {}",
        api_requests, probe_target
    );

    let probe_state = provider::FeedProbeState {
        newest_published_at: fetched.newest_published_at,
        source_fingerprint: Some(build_source_fingerprint(
            fetched.total_results,
            &fetched.first_page_ids,
        )),
        probe_target: Some(probe_target),
    };

    Ok(ApiFetch {
        channel: rss_channel,
        items: rss_items,
        probe_state,
        api_requests,
    })
}

/// The watch URL for a video id. Load-bearing as the Episode Guid format:
/// both the API path and the quota-free atom path emit this exact string as
/// the item guid (see docs/adr/0002 — the scheme is frozen identity, never
/// change it without acknowledging a mass duplication in every subscriber).
fn watch_url_for_video_id(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
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

fn build_channel_items_from_playlist(
    items: Vec<PlaylistItem>,
    videos_infos: HashMap<String, VideoExtraInfo>,
    filter_shorts: bool,
) -> Vec<Item> {
    let rss_item: Vec<Item> = items
        .into_iter()
        .filter_map(|item| {
            let mut snippet = item.snippet?;
            let title = snippet.title.take().unwrap_or("".to_owned());
            let description = snippet.description.take().unwrap_or("".to_owned());
            let video_id = snippet.resource_id.take()?.video_id?;
            let url = Url::parse(&watch_url_for_video_id(&video_id)).ok()?;

            let video_infos = videos_infos.get(&video_id).or_else(|| {
                warn!("no duration found for {:?}", video_id);
                None
            })?;

            // Calculate duration in seconds for filtering (fields are f32)
            let duration_seconds = (video_infos.duration.hour * 3600.0
                + video_infos.duration.minute * 60.0
                + video_infos.duration.second) as u64;

            // Filter out shorts if enabled (videos <= 3 minutes/180 seconds)
            if filter_shorts && duration_seconds <= SHORTS_THRESHOLD_SECONDS {
                debug!(
                    "filtering out short video: {} ({} seconds)",
                    video_id, duration_seconds
                );
                return None;
            }

            let mut item_builder = ItemBuilder::default();
            item_builder.title(Some(title));
            item_builder.description(Some(description.clone()));
            item_builder.link(Some(url.to_string()));
            item_builder.guid(Some(GuidBuilder::default().value(url.to_string()).build()));
            item_builder.pub_date(
                snippet
                    .published_at
                    .map(|pub_date| pub_date.to_rfc2822().to_string()),
            );
            item_builder.author(snippet.channel_title.clone());
            let itunes_item_extension = ITunesItemExtensionBuilder::default()
                .summary(Some(description))
                // Apple ignores plain <author>; episodes need itunes:author
                .author(snippet.channel_title.take())
                .duration(Some({
                    let hours = video_infos.duration.hour;
                    let minutes = video_infos.duration.minute;
                    let seconds = video_infos.duration.second;
                    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
                }))
                .image(get_thumb!(snippet).and_then(|t| t.url))
                .build();
            item_builder.itunes_ext(Some(itunes_item_extension));
            Some(item_builder.build())
        })
        .collect();
    rss_item
}

/// Result of fetching a playlist's items: the items sorted by publication
/// date, plus the raw source state needed to build a Freshness Probe state.
///
/// `total_results`, `first_page_ids` and `newest_published_at` are all derived
/// from the first page *in playlist position order* (before any sorting or
/// short filtering) so that a probe can re-derive them the exact same way and
/// compare like with like.
struct PlaylistFetch {
    items: Vec<PlaylistItem>,
    /// pageInfo.totalResults of the first page: total items in the playlist
    /// as reported by the API.
    total_results: Option<i32>,
    /// Video ids of the first page, in playlist position order (up to 50).
    first_page_ids: Vec<String>,
    /// Newest publication timestamp (unix seconds) within the first page.
    newest_published_at: Option<i64>,
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
    debug!("fetching items from playlist {}", playlist_id);
    loop {
        let remaining_items = max_fetched_items - fetched_playlist_items.len();
        let items_to_fetch = if remaining_items > 50 {
            50
        } else {
            remaining_items
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
    debug!("getting stream_url for yt video: {}", url);
    let extra_args: Vec<String> =
        serde_json::from_str(conf().get(ConfName::YoutubeYtDlpExtraArgs)?.as_str()).map_err(|_| eyre!(r#"failed to parse YOUTUBE_YT_DLP_GET_URL_EXTRA_ARGS allowed syntax is ["arg1#", "arg2", "arg3", ...]"#))?;
    let mut command = tokio::process::Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .arg("-f")
        .arg("bestaudio")
        .arg("--get-url")
        .arg(url.as_str());

    for arg in extra_args {
        command.arg(arg);
    }

    let output = tokio::time::timeout(Duration::from_secs(120), command.output()).await;

    match output {
        Ok(Ok(x)) => {
            let raw_url = std::str::from_utf8(&x.stdout).unwrap_or_default();
            match Url::from_str(raw_url) {
                Ok(url) => Ok(url),
                Err(e) => {
                    warn!(
                        "error while parsing stream url using yt-dlp:\nerror: {}\nyt-dlp stdout: {}\nyt-dlp stderr: {}",
                        e,
                        raw_url,
                        std::str::from_utf8(&x.stderr).unwrap_or_default()
                    );
                    Err(eyre::eyre!(e))
                }
            }
        }
        Ok(Err(e)) => Err(eyre::eyre!(e)),
        Err(_) => Err(eyre::eyre!("yt-dlp timed out after 120s")),
    }
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
            .connection_string(&conf().get(ConfName::RedisUrl).unwrap())
            .build()
            .await
            .expect("youtube_channel_username_to_id cache")
} "##
    )
)]
async fn find_yt_channel_url_with_c_id(url: &Url) -> eyre::Result<Url> {
    info!("conversion not in cache, using yt-dlp for conversion...");
    let mut command = Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .arg("--playlist-items")
        .arg("0")
        .arg("-O")
        .arg("playlist:channel_url")
        .arg(url.to_string());

    let output = tokio::time::timeout(Duration::from_secs(120), command.output()).await;
    let output = output.map_err(|_| eyre::eyre!("yt-dlp timed out after 120s"))??;
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
        .filter_map(|entry| {
            let link = entry.links.first().map(|d| d.clone().href);

            // Get duration for filtering
            let duration_seconds = (|| -> Option<u64> {
                duration_map
                    .get(&link.clone()?)
                    .and_then(|s| s.map(|a| a as u64))
            })();

            // Filter out shorts if enabled (videos <= 3 minutes/180 seconds)
            if filter_shorts {
                if let Some(seconds) = duration_seconds {
                    if seconds <= SHORTS_THRESHOLD_SECONDS {
                        debug!(
                            "filtering out short video from atom feed: {} ({} seconds)",
                            link.clone().unwrap_or_default(),
                            seconds
                        );
                        return None;
                    }
                }
            }

            let mut item_builder = ItemBuilder::default();
            item_builder.title(entry.title.map(|d| d.content));
            item_builder.description(
                entry
                    .media
                    .first()
                    .and_then(|d| Some(d.clone().description?.content)),
            );
            item_builder.link(link.clone());
            // Episode Guid must be identical to the one the API path emits for
            // the same video, or podcatchers re-identify every episode whenever
            // the Degradation Ladder serves this quota-free feed (mass
            // duplication). The API path guids are the watch URL.
            let guid = match entry.id.strip_prefix("yt:video:") {
                Some(video_id) => watch_url_for_video_id(video_id),
                None => link.clone().unwrap_or_else(|| entry.id.clone()),
            };
            item_builder.guid(Some(GuidBuilder::default().value(guid).build()));
            item_builder.pub_date(
                entry
                    .published
                    .or(entry.updated)
                    .map(|date| date.to_rfc2822().to_string()),
            );
            let mut itunes_item_builder = ITunesItemExtensionBuilder::default();
            if !channel_title.is_empty() {
                // Apple ignores plain <author>; episodes need itunes:author
                itunes_item_builder.author(Some(channel_title.clone()));
            }
            let media = entry.media.first();
            itunes_item_builder.image(
                media
                    .and_then(|m| m.thumbnails.first())
                    .map(|t| t.clone().image.uri),
            );
            let duration = duration_seconds
                .map(|a| format!("{:02}:{:02}:{:02}", a / 3600, a / 60 % 60, a % 60));
            itunes_item_builder.duration(duration);
            item_builder.itunes_ext(Some(itunes_item_builder.build()));
            Some(item_builder.build())
        })
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
            .connection_string(&conf().get(ConfName::RedisUrl).unwrap())
            .build()
            .await
            .expect("youtube_duration cache")
} "##
    )
)]
async fn get_youtube_video_duration_with_ytdlp(url: &Url) -> eyre::Result<Option<usize>> {
    debug!("getting duration for yt video: {}", url);

    let mut command = Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .arg("--get-duration")
        .arg(url.to_string());

    let output = tokio::time::timeout(Duration::from_secs(120), command.output()).await;

    match output {
        Ok(Ok(x)) => {
            let duration_str = std::str::from_utf8(&x.stdout).unwrap().trim().to_string();
            Ok(Some(
                parse_duration(&duration_str)
                    .unwrap_or_default()
                    .as_secs()
                    .try_into()
                    .unwrap(),
            ))
        }
        Err(_) => {
            warn!("yt-dlp duration fetch timed out after 120s");
            Ok(None)
        }
        Ok(Err(_)) => {
            warn!("could not parse youtube video duration");
            Ok(None)
        }
    }
}

fn parse_duration(duration_str: &str) -> Result<Duration, String> {
    let duration_parts: Vec<&str> = duration_str.split(':').rev().collect();

    let seconds = match duration_parts.first() {
        Some(sec_str) => sec_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    let minutes = match duration_parts.get(1) {
        Some(min_str) => min_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    let hours = match duration_parts.get(2) {
        Some(hour_str) => hour_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    let duration_secs = hours * 3600 + minutes * 60 + seconds;
    Ok(Duration::from_secs(duration_secs))
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
    fn test_build_source_fingerprint_is_order_sensitive() {
        let a = build_source_fingerprint(Some(2), &["vid1".to_string(), "vid2".to_string()]);
        let b = build_source_fingerprint(Some(2), &["vid2".to_string(), "vid1".to_string()]);
        assert_ne!(a, b);
    }

    #[test]
    fn test_probe_page_size_matches_conversion_first_page() {
        // default config: full page
        assert_eq!(probe_page_size(300), 50);
        // small YOUTUBE_MAX_RESULTS: the probe must fetch exactly what a
        // conversion would, or every probe would report "changed"
        assert_eq!(probe_page_size(13), 13);
        assert_eq!(probe_page_size(1), 1);
        assert_eq!(probe_page_size(49), 49);
        assert_eq!(probe_page_size(50), 50);
        // degenerate config values still produce a valid page size
        assert_eq!(probe_page_size(0), 1);
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
}
