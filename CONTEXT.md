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
- **Provider**: the per-platform strategy (YouTube, PeerTube, Generic)
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
- **Episode Metadata**: the per-Episode presentation fields a Feed carries —
  title, description, artwork, publish date — what a podcatcher renders for an
  Episode. Distinct from Episode Guid and Enclosure URL, which identify and
  deliver an Episode rather than present it.
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
- **Quota exhaustion (typed)**: a Feed Conversion failure the provider itself
  classifies and reports as a typed case. The ladder opens the Quota Breaker
  on this case by type, never by reading upstream error text.
- **Usage day**: the provider-owned day bucket metered usage is counted on
  (for YouTube: the Pacific-time date, matching its reset). The cache stores
  opaque buckets.
- **Degradation Ladder**: the fixed order of attempts when serving a Feed:
  Fresh cache → Stale cache → Feed Conversion → Degraded Feed → error. The
  Degraded rung applies when the Feed Conversion fails — quota exhaustion
  included, and it applies preemptively while the Quota Breaker is open. A
  rung is skipped only when it cannot produce a result.

## Source staging

- **Source Stage**: the act of fetching an Episode's source media to a local
  file with parallel bounded ranged GETs before ffmpeg reads it. Exists
  because the googlevideo edge serves bounded ranges at full speed while
  throttling open-ended reads to ~27 KB/s (measured 2026-10-16, issue #166).
- **Staged Source**: the local, byte-verified file a Source Stage produces.
  Byte-verified means every byte range was served in full against the
  **Origin Total**; ffmpeg never sees a staged file that failed verification.
  On the Open-ended Fallback there is no declared total — verification there
  is the body's clean end, and the received count is the truth.
- **Direct Media Source**: a provider whose resolved Stream URL is the media
  bytes themselves (YouTube's googlevideo URL) — the only kind a Source
  Stage can fetch. Manifest-based providers (PeerTube's
  streaming playlists) resolve to playlists and are never staged, whatever
  the kill switch says; the provider declares this (`streams_direct_media`).
- **Staging Kill Switch**: the `STAGE_SOURCES` config, default on — staged
  fetching is the production path. Only the exact rollback spellings
  ("false", "0", "off", "no" — case-insensitive, trimmed) return every
  request to the legacy path: the resolved Stream URL goes straight to
  ffmpeg with the TCP preflight. A typo of one of them keeps staging on:
  an unrecognized value is never read as a rollback.
- **Staging Slot**: the concurrency unit of Source Staging — one per Staged
  Source on disk, taken before the stage creates its temp file and held
  until the transcode deletes it. Slots are handed out by the **Staging
  Gate** (`stage_gate()` in code), the process-wide pool whose capacity is
  the **Staging Concurrency** (`STAGE_CONCURRENCY`, default 4); transcodes
  beyond it queue instead of staging in parallel, bounding egress and
  ephemeral storage alike. The gate is initialized exactly once at startup —
  beside the Startup Sweep, before the server accepts its first request —
  and a second initialization fails loudly. Tests build gates of their own
  capacity and inject them into the resolver. A slot is held only on the
  staged path: the kill switch, manifest-based sources and the legacy path
  go slotless.
- **Startup Sweep**: the process-start removal of staged temp files a
  crashed predecessor left. Runs once, before the server accepts its first
  request — so a live staged file is never swept — and only touches files
  under the staged-file prefix; guard-owned deletion covers everything
  created after it.
- **Check Stage**: the pipeline step consulted with the resolved Stream URL
  before ffmpeg runs. The legacy one is the TCP preflight (fail fast on a
  SYN-dropping edge); a staging one performs the Source Stage and hands
  back the Staged Source in place of the Stream URL. Either kind's failure
  evicts the cached resolution and fails the request fast.
- **Chunk**: one bounded ranged GET of a Source Stage, identified by the byte
  span it fetches (`ChunkSpan` in code). Bounded means an explicit start and
  end — the shape the edge serves at full speed. A stage runs
  `STAGE_CHUNK_CONCURRENCY` (default 4, the measured full-aggregate speed)
  Chunks at once; distinct from the **Staging Concurrency**, which bounds
  how many sources stage at the same time.
- **Chunk Plan**: the ordered list of Chunks tiling the source exactly,
  decided purely from the Origin Total and the chunk size. Pure decision
  code, no I/O — the fetcher executes it.
- **Origin Total**: the source byte count the first Chunk's Content-Range
  reports. That first request doubles as the **Source Probe**: a dead CDN
  edge fails it within the attempt budget, fast, the way the TCP preflight
  fails a dead resolution today.
- **Open-ended Fallback**: the fallback ladder's second rung (issue #171).
  When the Source Probe's ranged GET is answered with a 200 full body — the
  origin ignores Range and no Chunk Plan can be built — the Source Stage
  fetches the whole source with one open-ended GET into the same temp file,
  engaging on its own with no operator action. ffmpeg still reads locally;
  the attempt budget and the fast, clean failure rules are the ranged path's.
- **Scripted Origin**: the test fixture encoding the measured edge behavior
  (fast bounded ranges, throttled open-ended reads, stalled responses,
  mid-body drops, Range ignored, 5xx, Content-Range lies) so the throttle bug
  keeps a permanent regression test.
