//! The one Episode shaper both rung-producing paths call. Owns every
//! per-Episode field a Feed item carries — the Episode Metadata (title,
//! description, artwork, publish date), the Episode Guid, duration, and
//! author — plus the shorts filter, so the metered API path and the
//! quota-free atom path cannot drift apart on any of them.

use chrono::{DateTime, Utc};
use log::debug;
use rss::{extension::itunes::ITunesItemExtensionBuilder, GuidBuilder, Item, ItemBuilder};

use crate::provider::duration::format_duration;

/// Duration threshold for YouTube Shorts (180 seconds = 3 minutes)
const SHORTS_THRESHOLD_SECONDS: u64 = 180;

/// The per-Episode facts one rung-producing path extracts from its wire
/// format, ready for the shared shaper. Extraction cannot be shared (the
/// wire types differ); everything downstream of this struct is.
pub(super) struct EpisodeFacts {
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
    pub(super) guid: String,
    pub(super) link: Option<String>,
    pub(super) published_at: Option<DateTime<Utc>>,
    pub(super) duration_seconds: Option<u64>,
    pub(super) image_url: Option<String>,
    pub(super) author: Option<String>,
}

/// The two rung-producing sources of Episodes. They differ in what an
/// unknown duration means and in how much their wire format guarantees;
/// the shaper preserves those differences instead of unifying them
/// blindly — a divergence here is where the cross-rung guid contract
/// (docs/adr/0002) would silently break.
#[derive(Clone, Copy)]
pub(super) enum EpisodeSource {
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
pub(super) fn shape_episode(
    facts: EpisodeFacts,
    source: EpisodeSource,
    filter_shorts: bool,
) -> Option<Item> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
