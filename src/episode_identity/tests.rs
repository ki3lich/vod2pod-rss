use super::*;

// -- the cross-rung contract -------------------------------------------------
//
// ADR-0002's frozen equality: whichever rung the Degradation Ladder serves
// from, the same video must carry the same Episode Guid, or podcatchers
// re-identify every episode on every rung switch (mass duplication). Both
// rungs are driven through this module — the one home of the scheme — so the
// contract holds on every test invocation, with no API keys and no network.

#[test]
fn test_api_rung_and_atom_rung_derive_the_same_episode_guid() {
    for video_id in ["dQw4w9WgXcQ", "abc123", "0123456789_-"] {
        let api_guid = youtube_episode_guid(video_id);
        let atom_guid = youtube_episode_guid_from_atom(
            &format!("yt:video:{video_id}"),
            Some(&format!("https://www.youtube.com/watch?v={video_id}")),
        );
        assert_eq!(api_guid, atom_guid, "rungs disagree for video {video_id}");
        assert_eq!(
            api_guid,
            format!("https://www.youtube.com/watch?v={video_id}")
        );
    }
}

/// The enclosure uuid rides on the guid: equal guids across rungs imply equal
/// enclosure uuids, so a subscriber's downloads stay stable across rung
/// switches too.
#[test]
fn test_equal_guids_derive_equal_enclosure_uuids_across_rungs() {
    let video_id = "dQw4w9WgXcQ";
    let watch_url = format!("https://www.youtube.com/watch?v={video_id}");
    let api_uuid = enclosure_uuid(Some(&youtube_episode_guid(video_id)), &watch_url);
    let atom_uuid = enclosure_uuid(
        Some(&youtube_episode_guid_from_atom(
            &format!("yt:video:{video_id}"),
            Some(&watch_url),
        )),
        &watch_url,
    );
    assert_eq!(api_uuid, atom_uuid);
}

// -- the frozen guid schemes -------------------------------------------------

#[test]
fn test_episode_guid_is_the_watch_url() {
    // docs/adr/0002: the watch URL is frozen identity — never change it
    // without acknowledging a mass duplication in every subscriber
    assert_eq!(
        youtube_episode_guid("abc123"),
        "https://www.youtube.com/watch?v=abc123"
    );
}

#[test]
fn test_atom_entry_rekeys_onto_the_watch_url_scheme() {
    // the atom→API direction: atom ids are re-keyed because every existing
    // subscriber anchored on the API path's scheme
    assert_eq!(
        youtube_episode_guid_from_atom(
            "yt:video:abc123",
            Some("https://www.youtube.com/watch?v=SHOULD-NOT-BE-USED")
        ),
        "https://www.youtube.com/watch?v=abc123"
    );
}

#[test]
fn test_atom_entry_without_yt_prefix_keeps_the_link() {
    assert_eq!(
        youtube_episode_guid_from_atom(
            "some-other-scheme:xyz",
            Some("https://www.youtube.com/watch?v=xyz789")
        ),
        "https://www.youtube.com/watch?v=xyz789"
    );
}

#[test]
fn test_atom_entry_without_link_keeps_the_raw_entry_id() {
    // last resort: the guid is never empty
    assert_eq!(
        youtube_episode_guid_from_atom("some-other-scheme:xyz", None),
        "some-other-scheme:xyz"
    );
}

// -- Deterministic Enclosure URLs --------------------------------------------

/// Pin the derivation itself, not just its stability: the uuid must be
/// v5-of(guid) under the compiled-in namespace, so a regression back to a
/// random uuid (or a re-keying) fails loudly instead of silently drifting.
#[test]
fn test_enclosure_uuid_is_v5_of_the_episode_guid_under_the_compiled_in_namespace() {
    let expected = uuid::Uuid::new_v5(
        &EPISODE_ENCLOSURE_UUID_NAMESPACE,
        b"https://www.youtube.com/watch?v=abc",
    );
    assert_eq!(
        enclosure_uuid(Some("https://www.youtube.com/watch?v=abc"), "unused-link"),
        expected
    );
}

#[test]
fn test_guidless_item_seeds_from_its_source_link() {
    // an item without a guid seeds from its link — the enclosure points at
    // the link, so an item with neither cannot be transcoded at all and no
    // further fallback exists
    let link = "https://www.youtube.com/watch?v=abc";
    assert_eq!(
        enclosure_uuid(None, link),
        uuid::Uuid::new_v5(&EPISODE_ENCLOSURE_UUID_NAMESPACE, link.as_bytes())
    );
}

#[test]
fn test_distinct_seeds_derive_distinct_enclosure_uuids() {
    let first = enclosure_uuid(
        Some("https://www.youtube.com/watch?v=abc"),
        "https://www.youtube.com/watch?v=abc",
    );
    let second = enclosure_uuid(
        Some("https://www.youtube.com/watch?v=def"),
        "https://www.youtube.com/watch?v=def",
    );
    assert_ne!(
        first, second,
        "two episodes must not share an enclosure uuid"
    );
}

#[test]
fn test_guid_takes_precedence_over_the_link_as_seed() {
    // when a guid exists, the link must not leak into the seed: two items
    // with the same guid but different links are the same episode
    assert_eq!(
        enclosure_uuid(Some("https://www.youtube.com/watch?v=abc"), "unused-link"),
        enclosure_uuid(Some("https://www.youtube.com/watch?v=abc"), "another-link")
    );
}
