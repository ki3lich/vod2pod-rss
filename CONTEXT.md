# Domain Glossary

Ubiquitous language for vod2pod-rss. One term, one meaning. If code and glossary
disagree, one of them is wrong — fix it before moving on.

## Core concepts

- **Source URL**: the URL a podcatcher subscribes to through vod2pod (e.g. a
  YouTube playlist or channel URL). Many different Source URLs can point at the
  same underlying feed.
- **Feed**: the RSS document vod2pod serves for a Source URL. Produced by a
  **Feed Conversion**.
- **Feed Conversion**: the act of producing a Feed from a Source URL by talking
  to the upstream provider (e.g. the YouTube Data API). Costs metered provider
  resources (for YouTube: API quota units).
- **Provider**: the per-platform strategy (YouTube, Twitch, PeerTube, Generic)
  that knows how to convert Source URLs of its platform.
- **Transcoding**: converting a source media stream to a podcatcher-friendly
  audio stream on demand. Independent of Feed Conversion.

## Feed freshness

- **Freshness**: the property of a cached Feed that its cached copy is recent
  enough to be served as-is. Governed by the **Fresh TTL**, not by upstream
  state.
- **Freshness Probe**: a cheap upstream check (for YouTube: a single
  playlist-items call costing 1 quota unit) that decides whether the cached
  Feed still matches upstream, avoiding a full Feed Conversion. A probe can
  only answer "unchanged" or "changed"; it never produces a Feed.
- **Fresh Period**: the maximum age a cached Feed may reach while probes keep
  answering "unchanged". After it elapses a full Feed Conversion happens
  regardless of probe results. Bounds how long a silent upstream change
  (one a probe cannot see) can go unnoticed.
- **Stale Window**: the period after a Feed stopped being Fresh during which
  the cached copy may still be served — both while it is being revalidated in
  the background and when upstream cannot be reached. Serving within the
  Stale Window is always preferred over failing.
- **Stale Feed**: a cached Feed that is no longer Fresh but is inside the
  Stale Window.
- **Degraded Feed**: a Feed produced without the provider's metered API (for
  YouTube: the provider's own public atom feed, limited to the latest items).
  Fewer items and slower to produce than a normal Feed. Exists only so that
  subscribers never receive an error while quota lasts.

## Episode identity

- **Episode**: a single entry in a Feed — one upstream VoD (e.g. one YouTube
  video) as presented to a podcatcher.
- **Episode Guid**: the identifier a podcatcher uses to dedupe Episodes. One
  Episode has exactly one Episode Guid, independent of which Ladder rung
  produced the Feed that carried it (real conversion or Degraded Feed) and of
  when it was served. Changing the scheme re-identifies every Episode for
  every subscriber — a mass duplication — and is forbidden.
- **Enclosure URL**: the per-Episode audio URL a podcatcher fetches to play or
  download the Episode.
- **Deterministic Enclosure URL**: an Enclosure URL whose value for a given
  Episode is stable across serves and identical across all subscribers of the
  same instance. Required for conditional GET to be able to answer
  "unchanged"; a per-serve Enclosure URL makes the served Feed body differ on
  every fetch and defeats conditional GET.

## Cache identity

- **Canonical Feed Key**: the single cache identity for a Feed. Two different
  Source URLs that resolve to the same feed (e.g. `youtube.com/playlist?list=X`
  and `www.youtube.com/playlist?list=X`) MUST share one Canonical Feed Key, so
  they share freshness, staleness and quota cost.

## Quota

- **Quota**: the provider-metered budget (for YouTube: daily units, resetting
  at midnight Pacific time). Exhaustion means Feed Conversions fail upstream
  until reset.
- **Quota Breaker**: a shared, persistent flag that records "provider quota is
  known to be exhausted until time T". While open, no metered API call is
  attempted at all; requests fall through the Degradation Ladder instead.
- **Degradation Ladder**: the fixed order of attempts when serving a Feed:
  Fresh cache → Stale cache → Feed Conversion → Degraded Feed → error. The
  Degraded rung applies when the Feed Conversion fails — quota exhaustion
  included, and it applies preemptively while the Quota Breaker is open. A
  rung is skipped only when it cannot produce a result.
