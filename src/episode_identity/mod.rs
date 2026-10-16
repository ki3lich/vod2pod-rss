//! The Episode identity module: the one home of the Episode Guid scheme and
//! the Deterministic Enclosure URL derivation (docs/adr/0002 — both are
//! frozen identity: changing either re-identifies or re-downloads every
//! episode for every subscriber).
//!
//! Consumers: both YouTube rungs — the metered API path and the quota-free
//! atom path (the Degraded Feed rung) — obtain their Episode Guid here, and
//! the RSS transcodizer derives the injected enclosure uuid here. The
//! contract test in this module asserts both rungs derive the same Episode
//! Guid (and with it the same enclosure uuid) for the same video — the
//! cross-rung equality ADR-0002 freezes — and runs on every test invocation,
//! without API keys.

/// Fixed uuid-v5 namespace used to derive the per-episode enclosure uuid from
/// the episode identifier. NEVER change this value: enclosure urls are part
/// of episode identity for podcatchers (see CONTEXT.md,
/// "Deterministic Enclosure URL") and every derived uuid would change,
/// re-downloading every episode for every subscriber.
pub const EPISODE_ENCLOSURE_UUID_NAMESPACE: uuid::Uuid =
    uuid::uuid!("b3e1c9a0-4f2d-4e8a-9c1b-2d5f6a7b8c9d");

/// The Episode Guid for a YouTube video: the watch URL. The scheme is frozen
/// identity (docs/adr/0002): every existing subscriber anchored on it, and
/// any change re-identifies every episode — a mass duplication. Both YouTube
/// rungs emit this exact string as the item guid.
pub fn youtube_episode_guid(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

/// The Episode Guid a YouTube atom entry derives to. The entry id is
/// `yt:video:<video id>`: those re-key onto the API path's watch-URL scheme —
/// the atom→API direction ADR-0002 chose, because every existing subscriber
/// anchored on the API path. An entry under any other id scheme keeps its
/// link, falling back to the raw entry id when it has none, so the guid is
/// never empty.
pub fn youtube_episode_guid_from_atom(atom_entry_id: &str, link: Option<&str>) -> String {
    match atom_entry_id.strip_prefix("yt:video:") {
        Some(video_id) => youtube_episode_guid(video_id),
        None => link.unwrap_or(atom_entry_id).to_string(),
    }
}

/// The Deterministic Enclosure URL's uuid: uuid-v5 of the episode guid under
/// the compiled-in namespace. A guid-less item seeds from its source link —
/// the enclosure itself points at the link, so an item with neither cannot be
/// transcoded at all and no further fallback exists. Deterministic by
/// construction: every serve and every subscriber of the same episode derives
/// the same uuid, so the injected body (and its ETag) is stable and
/// conditional GET can answer "unchanged".
pub fn enclosure_uuid(episode_guid: Option<&str>, source_link: &str) -> uuid::Uuid {
    let seed = episode_guid.unwrap_or(source_link);
    uuid::Uuid::new_v5(&EPISODE_ENCLOSURE_UUID_NAMESPACE, seed.as_bytes())
}

#[cfg(test)]
mod tests;
