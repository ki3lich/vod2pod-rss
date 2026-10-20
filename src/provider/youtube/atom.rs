//! The quota-free atom path: the provider's own public atom feed (limited to
//! the latest items) with durations resolved through yt-dlp. Used both when
//! no API key is configured and as the quota-free rung of the Degradation
//! Ladder. Episode extraction happens here; shaping is the shared shaper's.

#[allow(unused_imports)]
use cached::macros::concurrent_cached;
#[allow(unused_imports)]
use cached::AsyncRedisCache;
use std::collections::HashMap;

use eyre::eyre;
use feed_rs::model::Feed;
use futures::stream::{self, StreamExt};
use log::{debug, info, warn};
use reqwest::Url;
use rss::{extension::itunes::ITunesChannelExtensionBuilder, ImageBuilder, Item};

use crate::configs::{conf, Conf, ConfName};
use crate::episode_identity;
use crate::provider;
use crate::provider::duration::parse_duration;
use crate::provider::yt_dlp;

use super::episode::{shape_episode, EpisodeFacts, EpisodeSource};
use super::identity::find_yt_channel_url_with_c_id;

/// Feed generation that never touches the metered YouTube Data API: the
/// provider's own public atom feed (limited to the latest items) with
/// durations resolved through yt-dlp. Used both when no API key is configured
/// and as the quota-free rung of the Degradation Ladder.
pub(super) async fn generate_rss_feed_via_atom(channel_url: Url) -> eyre::Result<String> {
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
}
