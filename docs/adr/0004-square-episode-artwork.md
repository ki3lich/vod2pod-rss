# 0004 — Episode artwork is composed square server-side

Date: 2026-11-23

## Status

Accepted. Amends ADR 0003: the artwork cache is explicitly exempt from its
fail-fast rule — artwork composes without Redis.

## Context

A YouTube Episode's artwork is the video's still, published as a 16:9 JPEG
(`maxresdefault` is 1280×720; the smaller variants `mqdefault` 320×180 and
`hqdefault` 480×360 — the last one already letterboxed by YouTube for
widescreen videos).

Apple Podcasts shows Episode Artwork in a square slot and crops anything else.
The Podcast Standards Project records this as the apps' most common ingestion
complaint — "artwork that isn't square and needs to be cropped" — and the
arithmetic is unforgiving: a 1280×720 still in a square slot keeps 56% of its
width, so **~22% is cut off each side**. Text and faces near the edges die.

Two ways to fix it feed-side were ruled out:

1. **Ask YouTube for a square.** Measured 2026-11-23: `?rs=300x300` and
   `?s=300x300` on `maxresdefault.jpg` return the original bytes, byte for
   byte. ytimg has no crop or resize parameter to use.
2. **Serve a letterboxed or cropped still.** Cropping is what the player
   already does (nothing gained); black bars violate Apple's "fill the entire
   template with art" and become unreadable in a 60 px list row.

ffmpeg — already a runtime dependency, one process per transcode — can compose
the square instead.

## Decision

1. **Feed items point their `itunes:image` at this service.** For a YouTube
   Episode whose provider already set artwork, the URL becomes
   `<feed origin>/artwork?id=<video id>`. Artwork a provider did not set is
   never invented: the player then falls back to the Show Cover, which is
   square already. Non-YouTube feeds (PeerTube, Twitch, generic) are untouched.
2. **The composition is ambient fill**, the pattern Apple's own artwork guide
   names ("contain the central subject within the art safe area… vague or
   ambient imagery should fill the rest of the template"): a 1400×1400 canvas,
   the whole still sharp at 92% of the canvas width and centered, over a copy
   of the same still scaled to cover, blurred (sigma 40) and darkened 15% so
   white playback controls stay legible. JPEG, quality 3. 1400 px is the
   smallest square Apple accepts and the largest our 1280 px source supports —
   3000 would be a fake upscale.
3. **Sources are 16:9 only**: `maxresdefault` then `mqdefault`. The 4:3
   variants are already letterboxed, so composing one produces bars inside the
   blur.
4. **Composition failure is not an error to the player.** The route answers
   `302` to `hqdefault.jpg`, which every video has: the player crops it exactly
   as it did before this route existed. A broken tile would be worse than a
   cropped picture.
5. **The route takes a video id and nothing else.** The upstream URL is built
   server-side from that id, so the route cannot be pointed at a host of the
   caller's choosing (the same SSRF concern the transcode route's whitelist
   guard answers).
6. **The square is cached, and the cache never decides service.** Redis holds
   the composed bytes under `vod2pod:artwork:v1:<id>` for 30 days; every Redis
   failure falls through to composing, so ADR 0003's hard-dependency rule does
   not extend to artwork. Responses carry
   `Cache-Control: public, max-age=31536000, immutable`: a square for one id
   never changes, so players fetch it once.
7. **`SQUARE_EPISODE_ARTWORK` is the kill switch** (`configs::enabled_from`,
   the spelling rule every kill switch here shares). Off means the feed carries
   the upstream thumbnail URL, exactly as before this decision.

## Consequences

- Every subscriber sees the whole still, uncropped, in the now-playing screen,
  lock screen, Control Center and episode pages — the placements Apple lists.
- One ffmpeg process per previously unseen Episode, ~0.3 s, plus one thumbnail
  fetch. Repeats are Redis (or the player's own cache) hits.
- The composition is part of the served feed's meaning: changing it (canvas
  size, blur, inset) means bumping `CACHE_PREFIX`'s version, and turning the
  feature off later changes what every existing subscriber's player shows.
  That is why the kill switch is an escape hatch, not a normal toggle.
- The still for the shownotes `<img>` keeps pointing upstream: HTML shows an
  image whole, so it has nothing to crop away.
