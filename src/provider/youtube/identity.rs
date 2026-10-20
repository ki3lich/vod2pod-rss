//! Canonical identity for the YouTube provider: the Canonical Feed Key
//! derivation and the yt-dlp handle→channel-id resolution every path
//! shares. Playlist and channel urls are read straight from the url; handle
//! and legacy urls resolve through the same (redis-cached) yt-dlp resolution
//! the feed conversions use, so no extra work is done on the happy path.

#[allow(unused_imports)]
use cached::macros::concurrent_cached;
#[allow(unused_imports)]
use cached::AsyncRedisCache;
use log::{info, warn};
use reqwest::Url;

#[allow(unused_imports)]
use crate::configs::{conf, Conf, ConfName};
use crate::provider::yt_dlp;

/// Canonical Feed Key identity for a youtube source url. Playlist and channel
/// urls are read straight from the url; handle and legacy urls are resolved to
/// their channel id through the same (redis-cached) yt-dlp resolution the feed
/// conversion uses, so no extra work is done on the happy path. Returns `None`
/// for urls this provider cannot canonically identify.
pub(super) async fn canonical_yt_feed_id(url: &Url) -> Option<String> {
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
pub(super) async fn find_yt_channel_url_with_c_id(url: &Url) -> eyre::Result<Url> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
