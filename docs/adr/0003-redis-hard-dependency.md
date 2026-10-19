# 0003 — Redis is a hard dependency: fail fast, reconnect, and let the readiness probe expose it

Date: 2026-10-19

## Status

Accepted. Amends ADR 0001: its serving guarantees hold while Redis is
reachable (see Consequences).

## Context

Redis backs every cache in the app: the feed cache (freshness, staleness,
quota breaker) and the yt-dlp lookups (stream URL, channel id, duration)
behind `#[concurrent_cached]`. On 2026-10-19 the production instance spent
~8 hours failing, and the failure class had three independent teeth:

1. **Connections never healed.** Feed serving opens a fresh Redis connection
   per request, so it healed on its own. The yt-dlp lookup caches are
   process-lifetime singletons built once around a plain multiplexed
   connection; when the Redis pod dropped their socket, every call failed
   with `redis error: broken pipe` until the app pod was manually restarted.
2. **Cache errors became outages.** The `cached` macro propagates cache-read
   errors through the wrapped function, so a dead cache turned into failed
   feed conversions (both ladder rungs — the quota-free rung uses the same
   lookups) and failed stream-URL resolution (downloads answered 503).
3. **The failure was invisible.** The k8s probes poll `/` — a static page —
   so a pod serving only errors looked perfectly healthy. The 503 path
   logged nothing. The outage surfaced only as podcatcher-side download
   failures and manual log archaeology.

The alternative — treat Redis as an optional accelerator and degrade to
uncached yt-dlp resolution when cache reads fail — was considered and
rejected: quota accounting (`bump_quota_usage`, the Quota Breaker) is also
Redis-backed, so "working without Redis" silently burns metered YouTube
quota while hiding the outage from the operator.

## Decision

1. **Redis stays a hard dependency.** A cache-read failure fails the
   operation it serves (feed conversion, stream-URL resolution); the app
   does not fake availability by degrading to uncached upstream work. With
   Redis down, quota accounting is lost, and burning quota invisibly is
   worse than failing loudly.
2. **Connections self-heal.** The yt-dlp lookup caches opt into `cached`'s
   auto-reconnecting connection manager, so a Redis blip no longer wedges
   the singletons until an app restart. A Redis restart no longer requires
   an app restart.
3. **The readiness probe checks Redis; liveness does not.** Plain `/health`
   stays process-only (the liveness probe); `/health?deep=1` pings Redis
   with a short timeout and answers 503 when unreachable. With Redis down
   the pod goes NotReady — visible in k8s within seconds — instead of
   crash-looping on a liveness check that a restart cannot fix. Single
   replica: the endpoint refuses connections while NotReady. Accepted for
   the sake of visibility.
4. **Failures are loud.** The transcoder-construction 503 path logs its
   cause, ending the silent-503 blind spot.

## Consequences

- ADR 0001's guarantees (stale over failing, Degraded Feeds, quota breaker)
  are scoped to "while Redis is reachable". With Redis unreachable, feeds
  and downloads fail fast and visibly instead of partially working.
- A Redis outage ends with a NotReady pod rather than a silently broken one;
  restoring Redis restores readiness. Restarting the app pod is no longer a
  required remediation step for this failure class.
- Podcatchers see connection errors during a Redis outage. Accepted: the
  operator sees the outage immediately, and podcatcher retry behavior
  (backoff over hours, observed in the 2026-10-19 logs) rides it out.
- The deep probe costs one connection + `PING` per readiness poll (10s
  period) — negligible.
- Known leftover: rust-analyzer (IDE) reports false E0308 coercion errors at
  the `serve()` call in `transcodize_rss`; rustc and CI accept the code.
