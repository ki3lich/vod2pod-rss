//! Server-composed square Episode Artwork.
//!
//! Apple Podcasts shows episode artwork in a square and crops anything else
//! (a 16:9 video still keeps 56% of its width, so ~22% goes off each side —
//! the complaint this module exists to fix). Apple's artwork guide asks for a square canvas
//! whose art fills it, with the subject inside the safe area so neither a
//! crop nor an overlaid control destroys it. A video still cannot be
//! redesigned per episode, so the composition follows Apple's own ambient-fill
//! language: the whole still, sharp, inside the safe area, and a blurred,
//! darkened copy of the same still filling the rest of the canvas. Every
//! pixel is art, nothing is transparent, nothing is cropped away.
//!
//! The composition is a pure function of the source still, so the result is
//! cached by video id: it never changes for a given episode.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{eyre, WrapErr};
use log::{debug, warn};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::configs::{conf, Conf, ConfName};

/// The canvas. Apple Podcasts accepts 1400×1400 to 3000×3000 and prefers the
/// largest, but the source still is at most 1280 px wide: anything bigger is
/// a fake upscale at several times the bytes. 1400 is the size Apple calls
/// valid and our source actually supports.
pub const CANVAS_PX: u32 = 1400;

/// The sharp still spans 92% of the canvas: Apple's template keeps design
/// elements inside the art safe area so overlaid controls and the app's own
/// corner rounding never land on something that matters.
const FRAME_WIDTH_PX: u32 = CANVAS_PX * 92 / 100;

/// Blur radius of the ambient fill, in ffmpeg's sigma units.
const BLUR_SIGMA: u32 = 40;

/// Darkening of the ambient fill (ffmpeg `eq` brightness, -1.0..1.0): white
/// playback controls sit on top of artwork, and a bright still would swallow
/// them.
const FILL_DARKEN: &str = "-0.15";

/// ffmpeg's JPEG quality scale (2 best … 31 worst).
const JPEG_QUALITY: u8 = 3;

/// One composition is a single ffmpeg run over one still; anything near this
/// budget means ffmpeg is wedged, not slow.
const COMPOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// YouTube's thumbnail host, keyed by video id. Building the URL here — never
/// taking a URL from the request — is what keeps this route free of the SSRF
/// hole the transcode route also guards against.
const UPSTREAM_THUMBNAIL_BASE: &str = "https://i.ytimg.com/vi";

/// The variants composed from, best first. 16:9 only: YouTube's 4:3 variants
/// (`hqdefault`, `sddefault`) are already letterboxed for widescreen videos,
/// so composing one would produce black bars *inside* the blur.
pub const SOURCE_VARIANTS: [&str; 2] = ["maxresdefault.jpg", "mqdefault.jpg"];

/// The redirect target when nothing can be composed. Every existing video has
/// this one; it is 4:3 and already letterboxed, which is why it is only ever
/// a fallback, never a source.
pub const FALLBACK_VARIANT: &str = "hqdefault.jpg";

/// Cache layout. The version is bumped whenever the composition itself
/// changes: a cached entry is never invalidated any other way, because the
/// square for a given id is otherwise immutable.
const CACHE_PREFIX: &str = "vod2pod:artwork:v1:";
const CACHE_TTL_SECS: u64 = 30 * 24 * 3600;

/// The kill-switch verdict for `SQUARE_EPISODE_ARTWORK`. Off means the feed
/// keeps the upstream thumbnail URL, exactly as it did before this module
/// existed.
pub fn enabled() -> bool {
    crate::configs::enabled_from(
        &conf()
            .get(ConfName::SquareEpisodeArtwork)
            .unwrap_or_default(),
    )
}

/// A YouTube video id, and nothing that could escape the URL path it is
/// pasted into: no `/`, no `.`, no `..`, no percent-decoding surprises.
pub fn is_video_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The upstream thumbnails tried in order, best first.
pub fn source_urls(video_id: &str) -> Vec<String> {
    SOURCE_VARIANTS
        .iter()
        .map(|variant| format!("{UPSTREAM_THUMBNAIL_BASE}/{video_id}/{variant}"))
        .collect()
}

/// Where a player is sent when the square cannot be produced.
pub fn fallback_url(video_id: &str) -> String {
    format!("{UPSTREAM_THUMBNAIL_BASE}/{video_id}/{FALLBACK_VARIANT}")
}

fn cache_key(video_id: &str) -> String {
    format!("{CACHE_PREFIX}{video_id}")
}

/// The ffmpeg filter graph: the still scaled to cover the canvas, cropped to
/// the square, blurred and darkened as the ambient fill; the same still scaled
/// to the frame width, laid over it, centered.
pub fn filter_graph() -> String {
    format!(
        "[0]scale={CANVAS_PX}:{CANVAS_PX}:force_original_aspect_ratio=increase,\
crop={CANVAS_PX}:{CANVAS_PX},gblur=sigma={BLUR_SIGMA},eq=brightness={FILL_DARKEN}[bg];\
[0]scale={FRAME_WIDTH_PX}:-2[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2"
    )
}

/// Where thumbnail bytes come from. Production fetches YouTube; tests install
/// a fake, the same way the transcode route's resolver seam is driven.
#[async_trait]
pub trait ThumbnailFetcher: Send + Sync {
    /// The bytes at `url`, or `None` when upstream has no such image.
    async fn fetch(&self, url: &str) -> eyre::Result<Option<Vec<u8>>>;
}

/// The production fetcher: one bounded GET per candidate thumbnail.
pub struct HttpThumbnailFetcher {
    client: reqwest::Client,
}

impl Default for HttpThumbnailFetcher {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl ThumbnailFetcher for HttpThumbnailFetcher {
    async fn fetch(&self, url: &str) -> eyre::Result<Option<Vec<u8>>> {
        let response = self.client.get(url).send().await?;
        if !response.status().is_success() {
            debug!("thumbnail {url} answered {}", response.status());
            return Ok(None);
        }
        Ok(Some(response.bytes().await?.to_vec()))
    }
}

/// The square artwork for `video_id`: cached bytes when Redis holds them,
/// otherwise fetched, composed and remembered. The cache is an optimisation
/// only — every Redis failure falls through to composing.
pub async fn square_artwork(
    video_id: &str,
    fetcher: &dyn ThumbnailFetcher,
) -> eyre::Result<Vec<u8>> {
    if let Some(cached) = cached_square(video_id).await {
        return Ok(cached);
    }
    let mut last_error = None;
    for url in source_urls(video_id) {
        match fetcher.fetch(&url).await {
            // a missing variant is the normal case: maxres does not exist for
            // every video, so move on to the next one
            Ok(None) => continue,
            Ok(Some(source)) => {
                let square = compose_square(&source).await?;
                remember_square(video_id, &square).await;
                return Ok(square);
            }
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no thumbnail variant exists for {video_id}")))
}

/// Compose one source still into the square canvas. ffmpeg is already a
/// runtime dependency of this service (it transcodes every episode), so the
/// composition adds no dependency — only one process per uncached artwork.
pub async fn compose_square(source: &[u8]) -> eyre::Result<Vec<u8>> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i", "pipe:0"])
        .args(["-filter_complex", &filter_graph()])
        .args(["-frames:v", "1", "-c:v", "mjpeg"])
        .args(["-q:v", &JPEG_QUALITY.to_string()])
        .args(["-f", "image2pipe", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .wrap_err("could not spawn ffmpeg to compose the square artwork")?;

    // the still goes in while the output is drained: a still can exceed the
    // pipe buffer, so writing it inline before waiting would deadlock
    let mut stdin = child.stdin.take().expect("ffmpeg stdin is piped");
    let input = source.to_vec();
    let writer = tokio::spawn(async move {
        // a failed write means ffmpeg died; its exit status reports why
        stdin.write_all(&input).await.ok();
    });

    let output = tokio::time::timeout(COMPOSE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            eyre!("ffmpeg timed out after {COMPOSE_TIMEOUT:?} composing the square artwork")
        })?
        .wrap_err("could not run ffmpeg to compose the square artwork")?;
    writer.await.ok();

    if !output.status.success() {
        return Err(eyre!(
            "ffmpeg failed composing the square artwork: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if output.stdout.is_empty() {
        return Err(eyre!(
            "ffmpeg produced no image composing the square artwork"
        ));
    }
    debug!(
        "composed a {CANVAS_PX}px square artwork from a {} byte still",
        source.len()
    );
    Ok(output.stdout)
}

async fn cached_square(video_id: &str) -> Option<Vec<u8>> {
    let mut con = crate::get_redis_client().await.ok()?;
    let cached: Option<Vec<u8>> = redis::cmd("GET")
        .arg(cache_key(video_id))
        .query_async(&mut con)
        .await
        .map_err(|e| warn!("could not read cached artwork for {video_id}: {e}"))
        .ok()?;
    cached
}

async fn remember_square(video_id: &str, square: &[u8]) {
    let Ok(mut con) = crate::get_redis_client().await else {
        warn!("no redis for the artwork cache; composing artwork for {video_id} every time");
        return;
    };
    if let Err(e) = redis::cmd("SET")
        .arg(cache_key(video_id))
        .arg(square)
        .arg("EX")
        .arg(CACHE_TTL_SECS)
        .query_async::<()>(&mut con)
        .await
    {
        warn!("could not cache artwork for {video_id}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::test_support::{jpeg_dimensions, still_16_9 as source_still, uncached_video_id};

    #[test]
    fn test_video_id_accepts_real_ids_and_nothing_path_shaped() {
        for good in ["dQw4w9WgXcQ", "a_b-C_d1234", "x"] {
            assert!(is_video_id(good), "{good:?} is a video id");
        }
        for bad in [
            "",
            "../etc/passwd",
            "id/../x",
            "id.jpg",
            "id%2f",
            "a b",
            &"x".repeat(65),
        ] {
            assert!(!is_video_id(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn test_source_chain_and_fallback_are_16_9() {
        let urls = source_urls("abc123");
        assert_eq!(
            urls,
            vec![
                "https://i.ytimg.com/vi/abc123/maxresdefault.jpg".to_string(),
                "https://i.ytimg.com/vi/abc123/mqdefault.jpg".to_string(),
            ],
            "the composition must only be built from 16:9 thumbnails"
        );
        assert_eq!(
            fallback_url("abc123"),
            "https://i.ytimg.com/vi/abc123/hqdefault.jpg"
        );
    }

    #[actix_rt::test]
    async fn test_compose_square_turns_a_16_9_still_into_the_accepted_canvas() {
        let square = compose_square(&source_still()).await.unwrap();
        assert_eq!(
            jpeg_dimensions(&square),
            (CANVAS_PX, CANVAS_PX),
            "Apple Podcasts accepts a square canvas only"
        );
        assert!(
            square.len() > 20_000,
            "a composed canvas is a real image, got {} bytes",
            square.len()
        );
        assert!(
            square.len() < 300_000,
            "a player downloads this per episode; keep a square under ~300 KB, got {} bytes",
            square.len()
        );
    }

    /// The cache is an optimisation, never a dependency (ADR 0003 stays about
    /// the feed cache): with Redis unreachable the square is still composed.
    #[actix_rt::test]
    async fn test_square_artwork_composes_when_redis_is_down() {
        let _env = crate::test_support::env_lock();
        let _address = crate::test_support::EnvPin::set("REDIS_ADDRESS", "127.0.0.1");
        let _port = crate::test_support::EnvPin::set("REDIS_PORT", "1");
        let video_id = uncached_video_id();
        let maxresdefault = source_urls(&video_id)[0].clone();
        let fetcher = FakeThumbnails::with(vec![(maxresdefault.as_str(), Vec::new())]);

        let square = square_artwork(&video_id, &fetcher)
            .await
            .expect("a dead Redis must not fail the artwork");

        assert_eq!(jpeg_dimensions(&square), (CANVAS_PX, CANVAS_PX));
    }

    #[actix_rt::test]
    async fn test_compose_square_refuses_an_input_that_is_not_an_image() {
        assert!(
            compose_square(b"not a jpeg").await.is_err(),
            "ffmpeg's refusal must surface as an error, not an empty image"
        );
    }

    struct FakeThumbnails {
        available: HashMap<String, Vec<u8>>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeThumbnails {
        fn with(stills: Vec<(&str, Vec<u8>)>) -> Self {
            let still = source_still();
            Self {
                available: stills
                    .into_iter()
                    .map(|(url, bytes)| {
                        (
                            url.to_string(),
                            if bytes.is_empty() {
                                still.clone()
                            } else {
                                bytes
                            },
                        )
                    })
                    .collect(),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ThumbnailFetcher for FakeThumbnails {
        async fn fetch(&self, url: &str) -> eyre::Result<Option<Vec<u8>>> {
            self.asked.lock().unwrap().push(url.to_string());
            Ok(self.available.get(url).cloned())
        }
    }

    #[actix_rt::test]
    async fn test_square_artwork_falls_through_to_the_next_variant() {
        // maxres missing, mqdefault present: the composition comes from the
        // later variant
        let video_id = uncached_video_id();
        let mqdefault = source_urls(&video_id)[1].clone();
        let fetcher = FakeThumbnails::with(vec![(mqdefault.as_str(), Vec::new())]);
        let square = square_artwork(&video_id, &fetcher).await.unwrap();
        assert_eq!(jpeg_dimensions(&square), (CANVAS_PX, CANVAS_PX));
        assert_eq!(
            fetcher.asked.lock().unwrap().len(),
            2,
            "the chain is tried in order, maxres first"
        );
    }

    #[actix_rt::test]
    async fn test_square_artwork_reports_when_no_variant_exists() {
        let fetcher = FakeThumbnails::with(vec![]);
        assert!(
            square_artwork(&uncached_video_id(), &fetcher)
                .await
                .is_err(),
            "no source still means no square: the route must fall back"
        );
    }
}
