//! The YouTube provider's quota typing: the breaker identity, the reset
//! time, the Usage day, and the mapping from upstream error bodies to the
//! typed `QuotaExhausted` case. The rest of the system only ever sees the
//! typed case — reading upstream error text is this module's concern alone.

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::America::Los_Angeles;

use crate::provider;

/// Quota breaker key of the YouTube provider: the identity under which the
/// Feed Serving module's cache adapters track this provider's quota
/// exhaustion. Provider-owned: the cache stores opaque breaker records.
pub(super) const YT_QUOTA_BREAKER_KEY: &str = "vod2pod:quota_breaker:youtube";

/// The moment YouTube's daily quota resets: midnight America/Los_Angeles.
/// The provider's answer to "when does your quota reset"; pure function so
/// tests can exercise it.
pub(super) fn next_quota_reset(now_utc: DateTime<Utc>) -> DateTime<Utc> {
    let pt_now = now_utc.with_timezone(&Los_Angeles);
    let next_day = pt_now.date_naive() + chrono::Duration::days(1);
    let midnight = next_day
        .and_hms_opt(0, 0, 0)
        .expect("midnight of any date exists");
    match Los_Angeles.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.with_timezone(&Utc)
        }
        // midnight is never skipped by US DST transitions; if that ever
        // changes, fall back to a conservative fixed PST offset (resets early
        // rather than late)
        chrono::LocalResult::None => {
            let approx = midnight - chrono::Duration::hours(8);
            DateTime::<Utc>::from_naive_utc_and_offset(approx, Utc)
        }
    }
}

/// Whether the upstream failure is the YouTube Data API refusing the call
/// because the project's daily quota is spent. YouTube's error vocabulary is
/// the provider's own concern: the rest of the system only ever sees the
/// typed `QuotaExhausted` conversion failure.
///
/// ceiling: detection is exactly as wide as the crate's JSON decoding. A
/// quota error whose body is not valid JSON arrives as `Error::Failure`,
/// carrying no structured reason to type against — identifying it would
/// require matching upstream error text, which the typed seam forbids — so
/// it degrades without opening the breaker.
fn is_youtube_quota_error(err: &eyre::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<google_youtube3::Error>(),
            Some(google_youtube3::Error::BadRequest(body))
                if is_quota_exceeded_body(body)
        )
    })
}

/// Whether a YouTube Data API error body carries the quota-exceeded reason.
fn is_quota_exceeded_body(body: &serde_json::Value) -> bool {
    body["error"]["errors"].as_array().is_some_and(|errors| {
        errors
            .iter()
            .any(|e| e["reason"].as_str() == Some("quotaExceeded"))
    })
}

/// The day bucket this provider's metered usage is currently counted on:
/// the Pacific-time date, matching the midnight-Pacific reset.
pub(super) fn quota_usage_day(now_utc: DateTime<Utc>) -> String {
    now_utc.with_timezone(&Los_Angeles).format("%F").to_string()
}

/// Map a conversion failure to the typed `QuotaExhausted` case when the
/// upstream error is YouTube quota exhaustion; any other failure passes
/// through untouched.
pub(super) fn youtube_quota_typed(err: eyre::Error) -> eyre::Error {
    if is_youtube_quota_error(&err) {
        eyre::Report::new(provider::QuotaExhausted::new(err))
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::youtube::YoutubeProvider;
    use crate::provider::MediaProvider;

    #[test]
    fn test_next_quota_reset_is_next_pacific_midnight() {
        // 2026-09-21T04:27Z is 2026-09-20 21:27 PDT (UTC-7): reset is
        // 2026-09-21 00:00 PDT == 07:00Z
        let now = DateTime::parse_from_rfc3339("2026-09-21T04:27:35Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-09-21T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(YoutubeProvider.next_quota_reset(now), Some(expected));
    }

    #[test]
    fn test_next_quota_reset_during_pst_winter() {
        // 2026-01-15T08:00Z is exactly midnight PST (UTC-8): reset is the
        // NEXT midnight, 2026-01-16 08:00Z
        let now = DateTime::parse_from_rfc3339("2026-01-15T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-01-16T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(YoutubeProvider.next_quota_reset(now), Some(expected));
    }

    #[test]
    fn test_next_quota_reset_day_before_dst_start() {
        // DST starts 2026-03-08 at 02:00; midnight of 2026-03-08 is still PST
        let now = DateTime::parse_from_rfc3339("2026-03-07T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-03-08T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(YoutubeProvider.next_quota_reset(now), Some(expected));
    }

    #[test]
    fn test_quota_usage_day_is_the_pacific_date() {
        // 2026-09-21T04:27Z is still 2026-09-20 in Pacific time
        let now = DateTime::parse_from_rfc3339("2026-09-21T04:27:35Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            YoutubeProvider.quota_usage_day(now),
            Some("2026-09-20".to_string())
        );
    }

    #[test]
    fn test_quota_breaker_key_is_provider_owned() {
        assert_eq!(
            YoutubeProvider.quota_breaker_key(),
            Some("vod2pod:quota_breaker:youtube".to_string())
        );
    }

    #[test]
    fn test_youtube_quota_error_maps_to_typed_exhaustion() {
        // the real body shape of a quota-exhausted YouTube Data API call
        let body = serde_json::json!({
            "error": {
                "code": 403,
                "errors": [
                    {"domain": "youtube.quota", "reason": "quotaExceeded"}
                ]
            }
        });
        let err = eyre::Report::from(google_youtube3::Error::BadRequest(body))
            .wrap_err("could not generate feed");

        let mapped = youtube_quota_typed(err);

        assert!(mapped.is::<provider::QuotaExhausted>());
        assert!(
            format!("{mapped:#}").contains("quotaExceeded"),
            "the upstream failure is preserved for diagnosis"
        );
    }

    #[test]
    fn test_other_conversion_errors_pass_through_untouched() {
        let mapped = youtube_quota_typed(eyre::eyre!("playlist not found"));

        assert!(!mapped.is::<provider::QuotaExhausted>());
        assert_eq!(format!("{mapped:#}"), "playlist not found");
    }
}
