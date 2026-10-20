//! The one owner of the byte-count formula (issue #184).
//!
//! ADR-0002 freezes the guarantee that the enclosure `length` attribute and
//! the served Content-Length agree; this module turns that one-formula
//! guarantee into one owner: both the RSS transcodizer (the advertised
//! enclosure length) and the transcode request module (the served window
//! arithmetic) call [`streamable_bytes`], so the two answers can never
//! diverge by drifting apart — there is only the one definition.

/// Bytes a transcode streams for `duration_secs` at `bitrate_kbps`
/// (constant bitrate: kbits/s × 1000 / 8 = bytes/s). The single source of
/// truth for the enclosure `length` attribute and the route's
/// Content-Length / Content-Range arithmetic, so the advertised size always
/// equals the served size (ADR-0002).
pub(crate) fn streamable_bytes(duration_secs: u64, bitrate_kbps: u64) -> u64 {
    duration_secs * bitrate_kbps * 1000 / 8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The formula's worked example, asserted as the test's own arithmetic —
    /// 2 s at 32 kbit/s is 2 × 32 × 1000 / 8 = 8000 bytes. The route-level
    /// pins (issue #183) reuse these exact numbers as literals, so a changed
    /// formula cannot sneak past either pin unnoticed.
    #[test]
    fn two_seconds_at_32_kbit_are_8000_bytes() {
        assert_eq!(streamable_bytes(2, 32), 8000);
    }
}
