//! The HTTP client for agentd's ctl API.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use core_types::{AttachRequest, AttachResponse, CtlError, CtlRequest};
use reqwest::header::CONTENT_LENGTH;
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;

use crate::DEFAULT_URL;

/// How long connecting to agentd may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a JSON request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an upload may take.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(300);

/// Why a request failed.
#[derive(Debug)]
pub enum Failure {
    /// agentd answered with a refusal or an error.
    Refused(CtlError),
    /// The request failed some way other than a refusal or no connection:
    /// it timed out, or agentd's answer was cut off or couldn't be read, so
    /// it may have reached agentd.
    Transport(String),
    /// agentd couldn't be connected to, so the request never reached it.
    Unreachable(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(err) => f.write_str(&err.message),
            Self::Transport(message) | Self::Unreachable(message) => f.write_str(message),
        }
    }
}

/// A client for one `AGENTCTL_URL` and `AGENTCTL_TOKEN`.
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: SecretString,
}

impl Client {
    /// A client for the API and token that `env` names.
    ///
    /// # Errors
    ///
    /// If `AGENTCTL_TOKEN` is missing, or `AGENTCTL_URL` isn't `http://`.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let token = env("AGENTCTL_TOKEN")
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .map(SecretString::from)
            .ok_or("AGENTCTL_TOKEN is not set")?;
        let base = env("AGENTCTL_URL")
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_URL.to_owned());
        let base = base.trim().trim_end_matches('/').to_owned();
        if !base.starts_with("http://") {
            return Err("AGENTCTL_URL must be an http:// URL".to_owned());
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|err| format!("can't build the HTTP client: {err}"))?;
        Ok(Self { http, base, token })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Sends `request` and returns agentd's answer.
    ///
    /// # Errors
    ///
    /// [`Failure::Refused`] with agentd's reason, or [`Failure::Transport`].
    pub async fn send<R: CtlRequest>(&self, request: &R) -> Result<R::Response, Failure> {
        self.send_within(request, REQUEST_TIMEOUT).await
    }

    /// Sends `request` and returns agentd's answer, giving up after `limit`
    /// or the usual request timeout, whichever is shorter.
    ///
    /// # Errors
    ///
    /// As for [`send`](Self::send).
    pub async fn send_within<R: CtlRequest>(
        &self,
        request: &R,
        limit: Duration,
    ) -> Result<R::Response, Failure> {
        let response = self
            .http
            .post(self.url(R::PATH))
            .bearer_auth(self.token.expose_secret())
            .json(request)
            .timeout(limit.min(REQUEST_TIMEOUT))
            .send()
            .await
            .map_err(|err| self.transport(&err))?;
        decode(response).await
    }

    /// Streams the file at `path` to agentd, named after its file name.
    ///
    /// # Errors
    ///
    /// If the file can't be read, or as for [`send`](Self::send).
    pub async fn attach(&self, path: &Path) -> Result<AttachResponse, Failure> {
        let name = path
            .file_name()
            .ok_or_else(|| Failure::Transport(format!("{} names no file", path.display())))?
            .to_str()
            .ok_or_else(|| Failure::Transport("the file name isn't valid UTF-8".to_owned()))?
            .to_owned();
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|err| Failure::Transport(format!("can't read {}: {err}", path.display())))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|err| Failure::Transport(format!("can't read {}: {err}", path.display())))?;
        if !metadata.is_file() {
            return Err(Failure::Transport(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let response = self
            .http
            .post(self.url(AttachRequest::PATH))
            .bearer_auth(self.token.expose_secret())
            .query(&AttachRequest { name })
            .header(CONTENT_LENGTH, metadata.len())
            .body(reqwest::Body::from(file))
            .timeout(ATTACH_TIMEOUT)
            .send()
            .await
            .map_err(|err| self.transport(&err))?;
        decode(response).await
    }

    fn transport(&self, err: &reqwest::Error) -> Failure {
        if err.is_connect() {
            return Failure::Unreachable(format!("agentd at {}: can't connect", self.base));
        }
        let what = if err.is_timeout() {
            "timed out"
        } else {
            "the request failed"
        };
        Failure::Transport(format!("agentd at {}: {what}", self.base))
    }
}

/// agentd's answer: the response on success, its [`CtlError`] otherwise.
async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, Failure> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|_| Failure::Transport("agentd's answer was cut off".to_owned()))?;
    if status.is_success() {
        return serde_json::from_slice(&body)
            .map_err(|_| Failure::Transport("agentd's answer couldn't be read".to_owned()));
    }
    match serde_json::from_slice::<CtlError>(&body) {
        Ok(err) => Err(Failure::Refused(err)),
        Err(_) => Err(Failure::Transport(format!(
            "agentd answered HTTP {}",
            status.as_u16()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use core_types::CtlErrorCode;

    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_url_defaults_to_agentctl_internal() {
        let client = Client::from_env(&env(&[("AGENTCTL_TOKEN", " tok\n")])).unwrap();
        assert_eq!(client.base, DEFAULT_URL);
        assert_eq!(client.token.expose_secret(), "tok");
        assert_eq!(
            client.url("/v1/post"),
            "http://agentctl.internal:8081/v1/post"
        );
    }

    #[test]
    fn the_url_is_taken_from_the_environment() {
        let client = Client::from_env(&env(&[
            ("AGENTCTL_TOKEN", "tok"),
            ("AGENTCTL_URL", "http://127.0.0.1:9/"),
        ]))
        .unwrap();
        assert_eq!(client.url("/v1/lock"), "http://127.0.0.1:9/v1/lock");
    }

    #[test]
    fn a_missing_token_or_a_non_http_url_is_refused() {
        for vars in [
            &[][..],
            &[("AGENTCTL_TOKEN", "  ")][..],
            &[("AGENTCTL_TOKEN", "t"), ("AGENTCTL_URL", "https://x")][..],
        ] {
            assert!(Client::from_env(&env(vars)).is_err(), "{vars:?}");
        }
    }

    #[test]
    fn failures_display_their_reason() {
        let refused = Failure::Refused(CtlError::new(CtlErrorCode::NoTurn, "no turn"));
        assert_eq!(refused.to_string(), "no turn");
        assert_eq!(Failure::Transport("down".into()).to_string(), "down");
        assert_eq!(Failure::Unreachable("gone".into()).to_string(), "gone");
    }
}
