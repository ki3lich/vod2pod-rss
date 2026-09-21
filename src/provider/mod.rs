mod generic;
#[macro_use]
mod macros;
mod peertube;
mod twitch;
mod youtube;

use async_trait::async_trait;
use log::debug;
use regex::Regex;
use reqwest::Url;
use rss::extension::itunes::ITunesChannelExtensionBuilder;
use serde::{Deserialize, Serialize};

use crate::configs::ConfName;
use crate::provider::{
    generic::GenericProvider, peertube::PeerTubeProvider, twitch::TwitchProvider,
    youtube::YoutubeProvider,
};

// to add a new provider just add it here (the provider should implement the MediaProvider trait)
generate_static_dispatcher!(
    Provider
    for
    YoutubeProvider,
    TwitchProvider,
    PeerTubeProvider,
    GenericProvider,
);

/// Provider-specific state captured at Feed Conversion time and stored next to
/// the cached feed, so a Freshness Probe can later compare upstream state
/// against what the feed was generated from, without regenerating.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct FeedProbeState {
    /// Publication timestamp of the newest item that went into the feed
    /// (unix seconds). `None` when the provider cannot determine it.
    pub newest_published_at: Option<i64>,
    /// Opaque, provider-defined fingerprint of the feed source state
    /// (for YouTube: total item count + ordered ids of the first playlist page).
    /// `None` when the provider cannot fingerprint its source.
    pub source_fingerprint: Option<String>,
    /// Provider-defined target the probe should query (for YouTube: the
    /// playlist id whose items feed the rss). `None` when the provider cannot
    /// probe.
    pub probe_target: Option<String>,
}

/// The result of a Feed Conversion: the feed itself plus the probe state
/// captured while producing it.
#[derive(Clone, Debug)]
pub struct GeneratedFeed {
    pub body: String,
    pub probe_state: Option<FeedProbeState>,
    /// Estimated metered API units consumed producing this feed (~1 unit per
    /// API call for YouTube). `None` when the provider is not metered.
    pub quota_units: Option<u64>,
}

#[async_trait]
pub trait MediaProvider {
    /// Given a channel will generate the full RRS feed
    ///
    /// # Arguments
    ///
    /// * `channel_url` - The URL of channel for wich the rss will be generated.
    async fn generate_rss_feed(&self, channel_url: Url) -> eyre::Result<GeneratedFeed>;

    /// Single cache identity for the feed behind `channel_url`, when the
    /// provider can determine it cheaply (without a full Feed Conversion).
    /// Two Source URLs that resolve to the same feed must yield the same
    /// identity so they share one cache entry. Returning `None` makes the
    /// caller fall back to a normalized form of the Source URL itself.
    async fn canonical_feed_id(&self, _channel_url: &Url) -> Option<String> {
        None
    }

    /// Freshness Probe: cheaply check whether the feed behind `channel_url`
    /// still matches `state` (captured at the last Feed Conversion).
    ///
    /// Returns `Ok(true)` when unchanged (the cache can be renewed without a
    /// new Feed Conversion), `Ok(false)` when changed, `Err` when the probe is
    /// unsupported or failed (the caller then falls back to a full conversion).
    /// The default reports "unsupported".
    async fn probe_feed_freshness(
        &self,
        _channel_url: &Url,
        _state: &FeedProbeState,
    ) -> eyre::Result<bool> {
        Err(eyre::eyre!(
            "freshness probe not supported by this provider"
        ))
    }

    /// Produce a feed without consuming any metered provider quota. Used as
    /// the last rung of the Degradation Ladder when the provider's metered
    /// path is unavailable (e.g. YouTube quota exhausted). The resulting feed
    /// is typically limited (fewer items) compared to `generate_rss_feed`.
    /// The default reports "unsupported".
    async fn generate_rss_feed_quota_free(&self, _channel_url: Url) -> eyre::Result<String> {
        Err(eyre::eyre!(
            "quota-free feed generation not supported by this provider"
        ))
    }

    /// Redis key of this provider's Quota Breaker, when the provider has a
    /// metered quota that can be exhausted. `None` for providers without one:
    /// their conversions are never skipped and never fall back. The default
    /// reports "no quota".
    fn quota_breaker_key(&self) -> Option<String> {
        None
    }

    /// Takes an URL and returns the stream URL, this will be passed to ffmpeg to start the
    /// transcoding process
    /// Only run when trancoding, if URL can't be converted to a streamable URL will return an error
    ///
    /// example: https://www.youtube.com/watch?v=UMO52N2vfk0 -> https://googlevideo.com/....
    ///
    /// for some provider conversion might not be needed, in that case just return the input
    ///
    /// # Arguments
    ///
    /// * `media_url` - The original URL found inside the RSS that should be streamed.
    async fn get_stream_url(&self, media_url: &Url) -> eyre::Result<Url>;

    /// Drop any cached resolved stream URL for `media_url`.
    ///
    /// Providers that cache the resolved stream URL (e.g. YouTube, which caches
    /// the yt-dlp googlevideo URL in Redis) override this so that when a stream
    /// URL is discovered to point at an unreachable source it is evicted and the
    /// next request re-resolves (the upstream CDN edge may have rotated). The
    /// default is a no-op for providers that do not cache stream URLs.
    ///
    /// Should be best-effort: implementations return `Ok(())` even if the cache
    /// is unavailable, since eviction failure must not mask the original error.
    async fn evict_stream_url_cache(&self, _media_url: &Url) -> eyre::Result<()> {
        Ok(())
    }

    /// Returns the regular expressions that will match all urls offered by the provider.
    /// This are the url associated with the provider
    /// es: for youtube you would need to match
    /// https://youtube\.com, https://youtu\.be, and https://.*\.googlevideo\.com/ (used to host the videos).
    ///
    /// if you need this to be user configurable then you need to create a ENV var, check the GenericProvider
    /// implementation for hints on how to do it
    ///
    /// # IMPORTANT
    /// this list is used to dinamically dispatch the provider, so the regex written here should
    /// never match what is already matched by other providers, if this for some reason is a huge
    /// limitation open an issue with a change request and why you need it to change.
    /// Also be warned missing a match here will cause the server to use the GenericProvider instead
    fn domain_whitelist_regexes(&self) -> Vec<Regex>;
}

/// Apple Podcasts show-level tags applied uniformly to every feed vod2pod
/// builds itself (Apple's "Podcaster's Guide to RSS"):
/// - `itunes:explicit` accepts only `true`/`false` — any other value (e.g.
///   `no`) is a hard validation error;
/// - `itunes:block` takes effect only when set to exactly `Yes`: it keeps
///   these feeds out of the public Apple directory, which is what a private
///   transcoder wants;
/// - `itunes:type` `episodic` matches VoD feeds (newest-first, no episode
///   numbering).
pub fn apply_apple_channel_tags(itunes: &mut ITunesChannelExtensionBuilder) {
    itunes.block(Some("Yes".to_string()));
    itunes.explicit(Some("false".to_string()));
    itunes.r#type(Some("episodic".to_string()));
}

/// This is the default rss structure used as a base for all the providers,
pub fn build_default_rss_structure() -> rss::ChannelBuilder {
    let mut feed_builder = rss::ChannelBuilder::default();

    let mut namespaces = std::collections::BTreeMap::new();
    namespaces.insert(
        "rss".to_string(),
        "http://www.itunes.com/dtds/podcast-1.0.dtd".to_string(),
    );
    namespaces.insert(
        "itunes".to_string(),
        "http://www.itunes.com/dtds/podcast-1.0.dtd".to_string(),
    );
    feed_builder.namespaces(namespaces);

    feed_builder.generator(Some("generated by vod2pod-rss".to_string()));

    // tell well-behaved podcatchers how often the feed is refreshed, in
    // minutes; this mirrors the Fresh TTL the server enforces server-side
    let fresh_ttl_seconds =
        crate::configs::conf_u64(ConfName::CacheTTL, crate::configs::DEFAULT_CACHE_TTL_SECS);
    feed_builder.ttl(Some((fresh_ttl_seconds / 60).max(1).to_string()));

    let mut itunes_section = ITunesChannelExtensionBuilder::default();
    apply_apple_channel_tags(&mut itunes_section);
    feed_builder.itunes_ext(Some(itunes_section.build()));

    feed_builder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_apple_channel_tags_sets_conformance_values() {
        let mut itunes = ITunesChannelExtensionBuilder::default();
        apply_apple_channel_tags(&mut itunes);
        let itunes = itunes.build();
        // Apple accepts only true/false for explicit
        assert_eq!(itunes.explicit.as_deref(), Some("false"));
        assert_eq!(itunes.block.as_deref(), Some("Yes"));
        assert_eq!(itunes.r#type.as_deref(), Some("episodic"));
    }

    #[test]
    fn test_default_rss_structure_carries_apple_tags() {
        let channel = build_default_rss_structure().build();
        let itunes = channel
            .itunes_ext
            .expect("default structure must have itunes_ext");
        assert_eq!(itunes.explicit.as_deref(), Some("false"));
        assert_eq!(itunes.block.as_deref(), Some("Yes"));
        assert_eq!(itunes.r#type.as_deref(), Some("episodic"));
    }
}
