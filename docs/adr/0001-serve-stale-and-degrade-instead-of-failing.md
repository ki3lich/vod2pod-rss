# 0001 — Serve stale feeds and degrade instead of failing on quota exhaustion

Date: 2026-09-21

## Status

Accepted

## Context

Podcatchers poll feeds on their own schedule. Every poll that misses the cache
triggers a full YouTube Data API Feed Conversion (~13 quota units at the
default settings). The free daily quota is 10,000 units. With aggressive
polling (10-minute cache TTL) this supports only a handful of feeds, and once
quota is exhausted the failure loop makes things worse:

1. Failed conversions are not cached, so every subsequent poll retries the full
   conversion and keeps burning quota on calls that fail anyway.
2. Podcatchers re-poll failed feeds aggressively, multiplying the retry
   pressure.
3. A podcatcher that repeatedly receives errors may drop the subscription.

The obvious fix — a longer cache TTL — only softens the math; it cannot make
10,000 units comfortable, and it does nothing about the failure loop.

## Decision

We change the serving policy instead of only tuning TTL:

1. **Serve stale within a bounded Stale Window** (default 7 days) whenever a
   Fresh copy is unavailable and upstream regeneration fails — including quota
   exhaustion. A stale feed is always better than an error.
2. **Revalidate in the background** (stale-while-revalidate): a stale hit is
   served immediately from cache; regeneration happens off the request path,
   coalesced per Canonical Feed Key so concurrent polls never duplicate API
   work.
3. **Probe before regenerating**: an expired-but-cached feed is checked with a
   1-unit Freshness Probe; a full Feed Conversion runs only when the probe
   says the feed changed, when the probe is unsupported, or when the Fresh
   Period (24h) is exceeded.
4. **Degrade, never error**: when no cached copy exists and quota is exhausted,
   the provider's quota-free path (YouTube's public atom feed) is used to
   produce a Degraded Feed. This is accepted knowing the feed may temporarily
   shrink to fewer items.
5. **Open a Quota Breaker** in shared storage when quotaExceeded is observed,
   so the instance stops attempting metered calls entirely until the provider's
   reset time (midnight Pacific).

## Consequences

- Subscribers effectively never see errors while any cached copy exists.
- The number of supported feeds becomes bounded by probe cost (~1 unit/hour
  per feed) instead of conversion cost (~13 units/poll), roughly an order of
  magnitude more feeds on the same quota.
- A deleted/private playlist may keep being served stale for up to the Stale
  Window before the error surfaces. Accepted.
- The Degraded Feed can shrink a feed to fewer items temporarily. Accepted.
- Probes have a documented blind spot (changes that leave both the total item
  count and the first playlist page untouched); the Fresh Period bounds how
  long such a change can stay invisible.
- The cache now stores data past its Fresh TTL, so cache entries and freshness
  markers are separate records.
