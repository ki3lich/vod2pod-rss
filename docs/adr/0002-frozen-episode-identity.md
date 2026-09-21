# 0002 — Episode identity is frozen: guid scheme never changes, enclosure URLs are deterministic

Date: 2026-09-21

## Status

Accepted

## Context

Podcatchers identify episodes by `<guid>` (falling back to the enclosure URL
when absent) and treat any guid they have not seen as a brand-new episode.
Two properties of vod2pod-rss make this identity fragile:

1. **Two guid schemes for the same episodes.** The YouTube API path emits the
   watch URL (`https://www.youtube.com/watch?v=<id>`), while the quota-free
   atom path emits the raw entry id (`yt:video:<id>`). Whenever the
   Degradation Ladder serves the atom path (quota exhausted past the cached
   copy, or any conversion failure), every episode in the feed carries a guid
   the podcatcher has never seen — a full duplicate set until the metered path
   returns.
2. **Per-serve enclosure URLs.** The injected enclosure URL embedded a fresh
   uuid4 on every serve. The served body — and therefore its ETag — changed on
   every fetch, so the conditional-GET path could never answer `304` for a
   transcoded feed, and two subscribers of the same feed received different
   enclosure URLs for the same episode.

The seemingly tidy fix for (1) — "unify the guids" — has a cost: any change of
guid scheme re-identifies every episode for every existing subscriber, a
one-time mass duplication in every podcatcher.

## Decision

1. **The guid scheme is frozen identity.** Both YouTube paths emit the watch
   URL guid. The atom path was aligned to the API path (atom→API direction)
   because every existing subscriber anchored on the API-path scheme; the
   reverse direction would have duplicated every subscription once. No future
   change to the guid scheme is acceptable without acknowledging a mass
   duplication.
2. **Enclosure URLs are deterministic.** The per-episode uuid is now uuid-v5
   derived from a fixed, compiled-in namespace and the episode guid (falling
   back to the source link — an item without a link cannot be transcoded at
   all, so no further fallback exists), so the injected body is
   byte-identical across serves and subscribers and conditional GET works as
   designed.
3. The enclosure `length` attribute and the server's Content-Length
   arithmetic share one formula (`streamable_bytes`), so the advertised size
   always equals the served size.

## Consequences

- Subscriptions that already saw degraded-feed duplicates keep those orphaned
  episodes; the duplicates stop recurring.
- `EPISODE_ENCLOSURE_UUID_NAMESPACE` and the guid formats are load-bearing
  constants: changing either re-identifies episodes or re-downloads every
  episode for every subscriber. They are documented as never-change values.
- Feed bodies are finally cacheable end-to-end: Apple's central crawler (and
  any conditional-GET client) gets `304` responses for unchanged feeds.
- episode guid stability across ladder rungs is asserted by unit tests on the
  atom conversion path.
