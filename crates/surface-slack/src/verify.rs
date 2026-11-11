//! Slack request signing: `v0` signatures over the raw body.
//!
//! Slack signs every request it sends an app with the app's signing secret:
//! `X-Slack-Signature: v0=<hex HMAC-SHA256(secret, "v0:{timestamp}:{body}")>`,
//! with the timestamp in `X-Slack-Request-Timestamp`. [`verify`] checks both
//! headers, the signature in constant time, and that the timestamp is within
//! [`MAX_CLOCK_SKEW_SECS`] of now, in either direction, so a captured request
//! can't be replayed later.

use axum::http::HeaderMap;
use hmac::{Hmac, KeyInit as _, Mac as _};
use secrecy::{ExposeSecret as _, SecretString};
use sha2::Sha256;

/// The header carrying the request's Unix timestamp in seconds.
pub const TIMESTAMP_HEADER: &str = "x-slack-request-timestamp";
/// The header carrying the `v0=` signature.
pub const SIGNATURE_HEADER: &str = "x-slack-signature";
/// How far a request's timestamp may be from now, in seconds, in either
/// direction: five minutes, as Slack recommends.
pub const MAX_CLOCK_SKEW_SECS: u64 = 300;

/// The longest timestamp header accepted, in digits. Unix seconds have 10
/// digits until the year 2286; 18 digits always fit an `i64`.
const MAX_TIMESTAMP_DIGITS: usize = 18;
/// The signature's version prefix.
const SIGNATURE_PREFIX: &[u8] = b"v0=";
/// An HMAC-SHA256 tag's length in bytes.
const TAG_LEN: usize = 32;

/// Why a request failed verification. None of the variants repeat a header
/// value or the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    /// A required header is missing.
    #[error("the {0} header is missing")]
    Missing(&'static str),
    /// A header that must appear once appears more than once.
    #[error("the {0} header appears more than once")]
    Duplicate(&'static str),
    /// The timestamp is not a plain decimal number of seconds.
    #[error("the timestamp is not a number of seconds")]
    BadTimestamp,
    /// The timestamp is more than [`MAX_CLOCK_SKEW_SECS`] from now.
    #[error("the timestamp is more than five minutes from now")]
    Stale,
    /// The signature is not `v0=` followed by 64 hex digits.
    #[error("the signature is not a v0 signature")]
    BadSignature,
    /// The signing secret is empty, so nothing can be verified with it.
    #[error("the signing secret is empty")]
    EmptySecret,
    /// The signature doesn't match the body, the timestamp and the secret.
    #[error("the signature does not match")]
    Mismatch,
}

/// Verifies a request's signature over its raw `body` with the app's
/// signing `secret`, taking `now` (Unix seconds) as the current time.
///
/// The body must be exactly the bytes received, before any parsing.
///
/// # Errors
///
/// A [`Rejection`] saying which check failed: a missing or repeated header,
/// a timestamp that isn't only decimal digits or is more than five minutes
/// from `now`, a malformed signature, or one that doesn't match. The tag is
/// compared in constant time.
pub fn verify(
    secret: &SecretString,
    headers: &HeaderMap,
    body: &[u8],
    now: i64,
) -> Result<(), Rejection> {
    let timestamp = single(headers, TIMESTAMP_HEADER)?;
    let signature = single(headers, SIGNATURE_HEADER)?;
    let seconds = parse_timestamp(timestamp)?;
    if seconds.abs_diff(now) > MAX_CLOCK_SKEW_SECS {
        return Err(Rejection::Stale);
    }
    let tag = parse_signature(signature)?;
    let key = secret.expose_secret().as_bytes();
    if key.is_empty() {
        return Err(Rejection::EmptySecret);
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| Rejection::EmptySecret)?;
    mac.update(b"v0:");
    mac.update(timestamp);
    mac.update(b":");
    mac.update(body);
    mac.verify_slice(&tag).map_err(|_| Rejection::Mismatch)
}

/// The value of a header that must appear exactly once.
fn single<'a>(headers: &'a HeaderMap, name: &'static str) -> Result<&'a [u8], Rejection> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(Rejection::Missing(name))?;
    if values.next().is_some() {
        return Err(Rejection::Duplicate(name));
    }
    Ok(value.as_bytes())
}

/// Parses a timestamp made only of ASCII digits: no sign, no spaces, no
/// fraction.
fn parse_timestamp(value: &[u8]) -> Result<i64, Rejection> {
    if value.is_empty()
        || value.len() > MAX_TIMESTAMP_DIGITS
        || !value.iter().all(u8::is_ascii_digit)
    {
        return Err(Rejection::BadTimestamp);
    }
    value.iter().try_fold(0_i64, |total, digit| {
        total
            .checked_mul(10)
            .and_then(|total| total.checked_add(i64::from(digit - b'0')))
            .ok_or(Rejection::BadTimestamp)
    })
}

/// Decodes `v0=` followed by exactly 64 hex digits into the 32-byte tag.
fn parse_signature(value: &[u8]) -> Result<[u8; TAG_LEN], Rejection> {
    let hex = value
        .strip_prefix(SIGNATURE_PREFIX)
        .ok_or(Rejection::BadSignature)?;
    if hex.len() != TAG_LEN * 2 {
        return Err(Rejection::BadSignature);
    }
    let mut tag = [0_u8; TAG_LEN];
    let (pairs, _) = hex.as_chunks::<2>();
    for (byte, [high, low]) in tag.iter_mut().zip(pairs) {
        *byte = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Ok(tag)
}

fn nibble(digit: u8) -> Result<u8, Rejection> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        b'A'..=b'F' => Ok(digit - b'A' + 10),
        _ => Err(Rejection::BadSignature),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
    const NOW: i64 = 1_531_420_618;
    const BODY: &[u8] = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
    const SIGNATURE: &str = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";

    fn secret() -> SecretString {
        SecretString::from(SECRET)
    }

    fn headers(timestamp: &str, signature: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(TIMESTAMP_HEADER, HeaderValue::from_str(timestamp).unwrap());
        headers.insert(SIGNATURE_HEADER, HeaderValue::from_str(signature).unwrap());
        headers
    }

    #[test]
    fn slacks_documented_example_verifies() {
        let headers = headers(&NOW.to_string(), SIGNATURE);
        assert_eq!(verify(&secret(), &headers, BODY, NOW), Ok(()));
    }

    #[test]
    fn testkit_signatures_verify() {
        let body = b"{\"type\":\"event_callback\"}";
        let signature = testkit::slack::sign(SECRET, NOW, body);
        let headers = headers(&NOW.to_string(), &signature);
        assert_eq!(verify(&secret(), &headers, body, NOW), Ok(()));
    }

    #[test]
    fn uppercase_hex_is_the_same_signature() {
        let upper = format!("v0={}", SIGNATURE[3..].to_ascii_uppercase());
        let headers = headers(&NOW.to_string(), &upper);
        assert_eq!(verify(&secret(), &headers, BODY, NOW), Ok(()));
    }

    #[test]
    fn a_changed_body_timestamp_or_secret_is_a_mismatch() {
        let good = headers(&NOW.to_string(), SIGNATURE);
        let mut body = BODY.to_vec();
        body.push(b'x');
        assert_eq!(
            verify(&secret(), &good, &body, NOW),
            Err(Rejection::Mismatch)
        );
        let other_secret = SecretString::from("another app's secret");
        assert_eq!(
            verify(&other_secret, &good, BODY, NOW),
            Err(Rejection::Mismatch)
        );
        let shifted = headers(&(NOW + 1).to_string(), SIGNATURE);
        assert_eq!(
            verify(&secret(), &shifted, BODY, NOW),
            Err(Rejection::Mismatch)
        );
        let padded = headers(&format!("0{NOW}"), SIGNATURE);
        assert_eq!(
            verify(&secret(), &padded, BODY, NOW),
            Err(Rejection::Mismatch)
        );
        let mut flipped = SIGNATURE.to_owned();
        flipped.replace_range(3..4, "b");
        let flipped = headers(&NOW.to_string(), &flipped);
        assert_eq!(
            verify(&secret(), &flipped, BODY, NOW),
            Err(Rejection::Mismatch)
        );
    }

    #[test]
    fn timestamps_more_than_five_minutes_away_are_stale() {
        let at = |timestamp: i64| {
            let signature = testkit::slack::sign(SECRET, timestamp, BODY);
            verify(
                &secret(),
                &headers(&timestamp.to_string(), &signature),
                BODY,
                NOW,
            )
        };
        assert_eq!(at(NOW - 300), Ok(()));
        assert_eq!(at(NOW + 300), Ok(()));
        assert_eq!(at(NOW - 301), Err(Rejection::Stale));
        assert_eq!(at(NOW + 301), Err(Rejection::Stale));
        assert_eq!(at(0), Err(Rejection::Stale));
    }

    #[test]
    fn missing_headers_are_named() {
        let mut only_signature = HeaderMap::new();
        only_signature.insert(SIGNATURE_HEADER, HeaderValue::from_static(SIGNATURE));
        assert_eq!(
            verify(&secret(), &only_signature, BODY, NOW),
            Err(Rejection::Missing(TIMESTAMP_HEADER))
        );
        let mut only_timestamp = HeaderMap::new();
        only_timestamp.insert(TIMESTAMP_HEADER, HeaderValue::from(NOW));
        assert_eq!(
            verify(&secret(), &only_timestamp, BODY, NOW),
            Err(Rejection::Missing(SIGNATURE_HEADER))
        );
    }

    #[test]
    fn repeated_headers_are_refused_even_if_one_is_valid() {
        let mut headers = headers(&NOW.to_string(), SIGNATURE);
        headers.append(SIGNATURE_HEADER, HeaderValue::from_static("v0=00"));
        assert_eq!(
            verify(&secret(), &headers, BODY, NOW),
            Err(Rejection::Duplicate(SIGNATURE_HEADER))
        );
        let mut headers = self::headers(&NOW.to_string(), SIGNATURE);
        headers.append(TIMESTAMP_HEADER, HeaderValue::from(NOW));
        assert_eq!(
            verify(&secret(), &headers, BODY, NOW),
            Err(Rejection::Duplicate(TIMESTAMP_HEADER))
        );
    }

    #[test]
    fn timestamps_must_be_plain_digits() {
        for bad in [
            "",
            "+1531420618",
            "-1531420618",
            " 1531420618",
            "1531420618 ",
            "1531420618.5",
            "1e9",
            "0x5b47e0ca",
            "abc",
            "9999999999999999999",
            "12345678901234567890123",
        ] {
            let headers = headers(bad, SIGNATURE);
            assert_eq!(
                verify(&secret(), &headers, BODY, NOW),
                Err(Rejection::BadTimestamp),
                "{bad:?}"
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            TIMESTAMP_HEADER,
            HeaderValue::from_bytes(b"15314\xff0618").unwrap(),
        );
        headers.insert(SIGNATURE_HEADER, HeaderValue::from_static(SIGNATURE));
        assert_eq!(
            verify(&secret(), &headers, BODY, NOW),
            Err(Rejection::BadTimestamp)
        );
    }

    #[test]
    fn signatures_must_be_v0_and_64_hex_digits() {
        let hex = &SIGNATURE[3..];
        for bad in [
            String::new(),
            hex.to_owned(),
            format!("v1={hex}"),
            format!("V0={hex}"),
            format!("v0={}", &hex[..63]),
            format!("v0={hex}0"),
            format!("v0={}g", &hex[..63]),
            format!("v0= {}", &hex[..63]),
            format!("v0={hex},v0={hex}"),
        ] {
            let headers = headers(&NOW.to_string(), &bad);
            assert_eq!(
                verify(&secret(), &headers, BODY, NOW),
                Err(Rejection::BadSignature),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn an_empty_secret_verifies_nothing() {
        let signature = testkit::slack::sign("", NOW, BODY);
        let headers = headers(&NOW.to_string(), &signature);
        assert_eq!(
            verify(&SecretString::from(""), &headers, BODY, NOW),
            Err(Rejection::EmptySecret)
        );
    }

    #[test]
    fn rejections_never_repeat_what_was_sent() {
        let rejections = [
            Rejection::Missing(TIMESTAMP_HEADER),
            Rejection::Duplicate(SIGNATURE_HEADER),
            Rejection::BadTimestamp,
            Rejection::Stale,
            Rejection::BadSignature,
            Rejection::EmptySecret,
            Rejection::Mismatch,
        ];
        for rejection in rejections {
            let text = rejection.to_string();
            assert!(!text.contains(&SIGNATURE[3..]), "{text}");
            assert!(!text.contains(SECRET), "{text}");
        }
    }
}
