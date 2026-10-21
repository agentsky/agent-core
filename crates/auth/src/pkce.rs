//! PKCE values, the authorize URL, and parsing what the member pastes back.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng as _;
use rand::rngs::SysRng;
use reqwest::Url;
use secrecy::zeroize::Zeroize as _;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};

use crate::AuthError;

/// Random bytes in a verifier and in a `state`.
const RANDOM_BYTES: usize = 32;

/// The longest paste [`parse_pasted`] looks at. A real `code#state` or
/// callback URL is a few hundred bytes.
const MAX_PASTE: usize = 4096;

/// `RANDOM_BYTES` bytes from the operating system's generator, base64url
/// without padding (43 characters).
fn random_token() -> Result<String, AuthError> {
    let mut bytes = [0u8; RANDOM_BYTES];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| AuthError::Random)?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    Ok(token)
}

/// A new PKCE code verifier.
pub(crate) fn new_verifier() -> Result<SecretString, AuthError> {
    random_token().map(SecretString::from)
}

/// A new OAuth `state`, drawn separately from the verifier, so it reveals
/// nothing about it.
pub(crate) fn new_state() -> Result<String, AuthError> {
    random_token()
}

/// The S256 code challenge for `verifier` (RFC 7636 section 4.2).
pub(crate) fn challenge(verifier: &SecretString) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.expose_secret().as_bytes()))
}

/// The authorize URL, with the query parameters in the order Claude Code
/// 2.1.285 writes them.
pub(crate) fn authorize_url(
    base: &Url,
    client_id: &str,
    redirect_uri: &Url,
    scope: &str,
    challenge: &str,
    state: &str,
) -> Url {
    let mut url = base.clone();
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri.as_str())
        .append_pair("scope", scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url
}

/// A parsed paste: the authorization code and the `state` it came back with.
pub(crate) struct Pasted {
    pub(crate) code: SecretString,
    pub(crate) state: String,
}

/// Parses what the member pasted: `code#state` as the callback page shows
/// it, or the callback URL itself (`…?code=…&state=…`).
///
/// Whitespace anywhere is dropped, since chat clients wrap long lines, and
/// so are backticks, quotes and angle brackets around the text, which chat
/// formatting adds. A Slack link written `<url|label>` keeps only the URL.
pub(crate) fn parse_pasted(pasted: &SecretString) -> Result<Pasted, AuthError> {
    let raw = pasted.expose_secret();
    if raw.len() > MAX_PASTE {
        return Err(AuthError::MalformedCode);
    }
    let mut compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let result = parse_compact(&compact);
    compact.zeroize();
    result
}

fn parse_compact(compact: &str) -> Result<Pasted, AuthError> {
    let trimmed = compact.trim_matches(|c| matches!(c, '`' | '"' | '\'' | '<' | '>'));
    let (code, state) = if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        let url = trimmed.split('|').next().unwrap_or(trimmed);
        from_callback_url(url)?
    } else {
        let (code, state) = trimmed.split_once('#').ok_or(AuthError::MalformedCode)?;
        (code.to_owned(), state.to_owned())
    };
    if !is_token(&code) || !is_token(&state) {
        return Err(AuthError::MalformedCode);
    }
    Ok(Pasted {
        code: SecretString::from(code),
        state,
    })
}

fn from_callback_url(url: &str) -> Result<(String, String), AuthError> {
    let url = Url::parse(url).map_err(|_| AuthError::MalformedCode)?;
    let mut code = None;
    let mut state = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" | "state" => return Err(AuthError::MalformedCode),
            _ => {}
        }
    }
    let code = code.ok_or(AuthError::MalformedCode)?;
    let state = match state {
        Some(state) => state,
        None => url.fragment().ok_or(AuthError::MalformedCode)?.to_owned(),
    };
    Ok((code, state))
}

/// Whether `value` can be a code or a state: non-empty, printable ASCII,
/// and none of the characters that separate the parts of a paste.
fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'#' | b'&' | b'?' | b'=' | b'|'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_result(text: &str) -> Result<(String, String), AuthError> {
        parse_pasted(&SecretString::from(text))
            .map(|pasted| (pasted.code.expose_secret().to_owned(), pasted.state))
    }

    fn parse(text: &str) -> Option<(String, String)> {
        parse_result(text).ok()
    }

    fn ok(code: &str, state: &str) -> Option<(String, String)> {
        Some((code.to_owned(), state.to_owned()))
    }

    #[test]
    fn tokens_are_43_base64url_characters_and_distinct() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 43);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_ne!(a, b);
    }

    #[test]
    fn challenge_matches_rfc_7636_appendix_b() {
        let verifier = SecretString::from("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            challenge(&verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorize_url_has_every_parameter_in_order() {
        let url = authorize_url(
            &Url::parse("https://claude.com/cai/oauth/authorize").unwrap(),
            "client",
            &Url::parse("https://platform.claude.com/oauth/code/callback").unwrap(),
            "user:profile user:inference",
            "CHALLENGE",
            "STATE",
        );
        assert_eq!(
            url.as_str(),
            "https://claude.com/cai/oauth/authorize?code=true&client_id=client\
             &response_type=code\
             &redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback\
             &scope=user%3Aprofile+user%3Ainference&code_challenge=CHALLENGE\
             &code_challenge_method=S256&state=STATE"
        );
    }

    #[test]
    fn parses_code_hash_state() {
        assert_eq!(parse("abc#xyz"), ok("abc", "xyz"));
    }

    #[test]
    fn tolerates_whitespace_anywhere() {
        assert_eq!(parse("  abc#xyz\n"), ok("abc", "xyz"));
        assert_eq!(parse("\tab c#\r\nxy z "), ok("abc", "xyz"));
    }

    #[test]
    fn tolerates_chat_formatting_around_the_text() {
        assert_eq!(parse("`abc#xyz`"), ok("abc", "xyz"));
        assert_eq!(parse("```abc#xyz```"), ok("abc", "xyz"));
        assert_eq!(parse("\"abc#xyz\""), ok("abc", "xyz"));
    }

    #[test]
    fn accepts_a_pasted_callback_url() {
        let url = "https://platform.claude.com/oauth/code/callback?code=abc&state=xyz";
        assert_eq!(parse(url), ok("abc", "xyz"));
        assert_eq!(parse(&format!(" <{url}> ")), ok("abc", "xyz"));
        assert_eq!(parse(&format!("<{url}|{url}>")), ok("abc", "xyz"));
        assert_eq!(
            parse("https://platform.claude.com/oauth/code/callback?state=xyz&code=a%2Db"),
            ok("a-b", "xyz")
        );
        assert_eq!(
            parse("https://platform.claude.com/oauth/code/callback?code=abc#xyz"),
            ok("abc", "xyz")
        );
    }

    #[test]
    fn rejects_malformed_pastes() {
        for text in [
            "",
            "   ",
            "abc",
            "abc#",
            "#xyz",
            "abc#xyz#more",
            "abc#x=y",
            "a\u{e9}c#xyz",
            "https://platform.claude.com/oauth/code/callback",
            "https://platform.claude.com/oauth/code/callback?code=abc",
            "https://platform.claude.com/oauth/code/callback?state=xyz",
            "https://platform.claude.com/oauth/code/callback?code=a&code=b&state=xyz",
            "https://platform.claude.com/oauth/code/callback?code=&state=xyz",
            "https://[not a url",
        ] {
            assert!(
                matches!(parse_result(text), Err(AuthError::MalformedCode)),
                "{text:?}"
            );
        }
        assert!(matches!(
            parse_result(&format!("{}#xyz", "a".repeat(MAX_PASTE))),
            Err(AuthError::MalformedCode)
        ));
    }

    #[test]
    fn the_error_never_repeats_the_paste() {
        let err = parse_result("secret-code-value").unwrap_err();
        assert!(!format!("{err} {err:?}").contains("secret-code-value"));
    }
}
