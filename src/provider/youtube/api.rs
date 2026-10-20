//! The metered YouTube Data API path: the full Feed Conversion (playlist or
//! channel → rss channel + items), the pagination it fetches with, the
//! Freshness Probe state it captures, and the video-info batches that cost
//! ~1 quota unit each. Episode extraction happens here; shaping is the
//! shared shaper's.

use std::collections::HashMap;

use futures::future::join_all;
use google_apis_common::NoToken;
use google_youtube3::{
    api::{self, PlaylistItem},
    hyper_rustls, YouTube,
};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use log::{debug, info, warn};
use reqwest::Url;
use rss::{extension::itunes::ITunesChannelExtensionBuilder, Channel, ChannelBuilder, Item};

use eyre::eyre;

use crate::configs::{conf, Conf, ConfName};
use crate::episode_identity;
use crate::provider;

use super::episode::{shape_episode, EpisodeFacts, EpisodeSource};
use super::identity::find_yt_channel_url_with_c_id;

pub(super) enum IdType {
    Playlist(String),
    Channel(String),
}

/// The Source URL a metered Feed Conversion starts from, as the id the
/// YouTube Data API addresses: a playlist id, or a channel id (handle and
/// legacy urls resolve through yt-dlp first).
pub(super) async fn id_from_url(channel_url: &Url) -> eyre::Result<IdType> {
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
            let url = find_yt_channel_url_with_c_id(channel_url).await?;
            let channel_id = url.path_segments().unwrap().next_back().unwrap();
            IdType::Channel(channel_id.into())
        }
        _ => return Err(eyre!("unsupported youtube url")),
    };
    Ok(id)
}

/// Everything a full YouTube API Feed Conversion produces: the rss channel +
/// items, the Freshness Probe state captured while fetching, and the number of
/// API requests spent (each costs ~1 quota unit).
pub(super) struct ApiFetch {
    pub(super) channel: Channel,
    pub(super) items: Vec<Item>,
    pub(super) probe_state: provider::FeedProbeState,
    pub(super) api_requests: usize,
}

pub(super) async fn fetch_from_api(
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

    let video_infos = join_all(videos_requests)
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
pub(super) fn probe_state_unchanged(
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
pub(super) struct ProbePage {
    pub(super) total_results: Option<i32>,
    pub(super) first_page_ids: Vec<String>,
    pub(super) newest_published_at: Option<i64>,
}

/// One ~1 quota unit call: the playlist's first page in position order plus
/// the playlist's total item count. Everything a probe needs to compare
/// against the state captured at Feed Conversion time.
///
/// The page size is not derived here: it is the shape the conversion's
/// pagination recorded next to its snapshot (`FeedProbeState::probe_page_size`),
/// so a probe's fingerprint is always derived from a page of the same shape
/// the conversion captured.
pub(super) async fn fetch_probe_page(
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

    // -- Episode identity: guids and pubDates must be identical across the API
    // path and the quota-free atom path, or podcatchers re-identify every
    // episode whenever the Degradation Ladder switches rungs (mass duplication).

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
