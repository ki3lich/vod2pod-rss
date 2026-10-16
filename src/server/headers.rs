//! Response-header mechanics of the HTTP adapter: ETags and HTTP-dates for
//! conditional GET (RFC 7232 / RFC 7231). These are client-facing response
//! concerns, not storage concerns; the cache module stays generic.

/// FNV-1a 64 bit: tiny and stable across processes and releases. Only used
/// for change detection (ETags), never for anything security related.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// ETag of a feed body: unquoted hex of the FNV-1a 64 hash.
pub fn etag_of(body: &str) -> String {
    format!("{:016x}", fnv1a64(body.as_bytes()))
}

/// RFC 7232 `If-None-Match` matching against our unquoted-hex etag. Handles
/// quoted values, weak validators (`W/`), comma separated lists and `*`.
pub fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    if_none_match.split(',').any(|candidate| {
        let candidate = candidate.trim();
        if candidate == "*" {
            return true;
        }
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        candidate.trim_matches('"') == etag
    })
}

/// RFC 7231 IMF-fixdate ("Sun, 21 Sep 2026 04:27:35 GMT") for a unix
/// timestamp, used in `Last-Modified` responses. Pure function so tests can
/// exercise it.
pub fn http_date(epoch_secs: i64) -> String {
    match chrono::DateTime::<chrono::Utc>::from_timestamp(epoch_secs, 0) {
        Some(dt) => dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        None => String::new(),
    }
}

/// Whether an `If-Modified-Since` HTTP-date is at or after `epoch_secs`
/// (meaning: the client already has this version, a 304 is appropriate).
/// Unparseable dates are treated as "client has nothing" (serve 200).
/// Pure function so tests can exercise it.
pub fn http_date_at_or_after(if_modified_since: &str, epoch_secs: i64) -> bool {
    // RFC 7231 IMF-fixdate always ends in a literal " GMT", which is UTC
    match chrono::NaiveDateTime::parse_from_str(if_modified_since, "%a, %d %b %Y %H:%M:%S GMT") {
        Ok(parsed) => parsed.and_utc().timestamp() >= epoch_secs,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_etag_of_is_deterministic_and_change_sensitive() {
        let a = etag_of("<rss>feed v1</rss>");
        assert_eq!(a, etag_of("<rss>feed v1</rss>"));
        assert_ne!(a, etag_of("<rss>feed v2</rss>"));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn test_etag_matches_rfc7232_forms() {
        let etag = etag_of("body");
        assert!(etag_matches(&format!("\"{etag}\""), &etag));
        assert!(etag_matches(&etag, &etag));
        assert!(etag_matches(&format!("W/\"{etag}\""), &etag));
        assert!(etag_matches(&format!("\"other\", \"{etag}\""), &etag));
        assert!(etag_matches(&format!("\"other\" ,\"{etag}\" "), &etag));
        assert!(etag_matches("*", &etag));
        assert!(!etag_matches("\"other\"", &etag));
        assert!(!etag_matches("", &etag));
    }

    #[test]
    fn test_http_date_roundtrip_and_comparison() {
        let date = http_date(1_789_964_855);
        assert_eq!(date, "Mon, 21 Sep 2026 04:27:35 GMT");
        // a client whose copy is as new as ours gets a 304
        assert!(http_date_at_or_after(&date, 1_789_964_855));
        // a client with an older copy must get a 200
        assert!(!http_date_at_or_after(&date, 1_789_964_856));
        assert!(http_date_at_or_after(&date, 1_789_964_854));
    }

    #[test]
    fn test_http_date_at_or_after_rejects_garbage() {
        assert!(!http_date_at_or_after("not a date", 0));
        assert!(!http_date_at_or_after("", 0));
    }
}
