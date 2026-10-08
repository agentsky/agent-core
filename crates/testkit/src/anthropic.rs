//! [`fake_anthropic`]: a local stand-in for the Anthropic API.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

pub use wiremock::Request;

/// The reply text used when no reply is queued.
pub const DEFAULT_REPLY: &str = "Hello from fake_anthropic.";

/// Starts a [`FakeAnthropic`] on a free local port.
pub async fn fake_anthropic() -> FakeAnthropic {
    FakeAnthropic::start().await
}

/// A `wiremock` server that answers like the Anthropic API, as far as the
/// Claude Code CLI needs.
///
/// - `HEAD /api/hello`, the CLI's connectivity check, gets a 200.
/// - `POST /v1/messages` (any query string, such as the CLI's `?beta=true`)
///   gets a message whose text is the next reply queued with
///   [`push_reply`](Self::push_reply), or [`DEFAULT_REPLY`]. A request
///   with `"stream": true` gets it as an SSE stream: `message_start`,
///   `content_block_start`, one `content_block_delta` with a `text_delta`,
///   `content_block_stop`, `message_delta` with `stop_reason: end_turn` and
///   `usage`, and `message_stop`. Any other request gets the same message as
///   JSON. A body that isn't JSON gets a 400 `invalid_request_error`.
/// - Anything else gets a 404, unless a mock added with
///   [`register`](Self::register) answers it.
///
/// Every request is recorded with its headers and body, whatever the
/// answer; see [`requests`](Self::requests).
#[derive(Debug)]
pub struct FakeAnthropic {
    server: MockServer,
    replies: Arc<Mutex<VecDeque<String>>>,
}

impl FakeAnthropic {
    /// Starts the server on a free local port.
    pub async fn start() -> Self {
        Self::serve(MockServer::start().await).await
    }

    /// Starts the server on `listener`, for a fake at an address fixed in
    /// advance.
    pub async fn start_on(listener: std::net::TcpListener) -> Self {
        Self::serve(MockServer::builder().listener(listener).start().await).await
    }

    async fn serve(server: MockServer) -> Self {
        let replies = Arc::new(Mutex::new(VecDeque::new()));
        Mock::given(method("HEAD"))
            .and(path("/api/hello"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(Messages {
                replies: Arc::clone(&replies),
            })
            .mount(&server)
            .await;
        Self { server, replies }
    }

    /// Adds `mock` to the server, so a request it matches gets its answer
    /// instead of a 404. It doesn't change the answers above.
    pub async fn register(&self, mock: Mock) {
        self.server.register(mock).await;
    }

    /// The base URL, `http://127.0.0.1:<port>`, for `ANTHROPIC_BASE_URL`.
    pub fn uri(&self) -> String {
        self.server.uri()
    }

    /// Queues the text of a later `/v1/messages` answer. Answers use queued
    /// replies in order, then [`DEFAULT_REPLY`].
    pub fn push_reply(&self, text: impl Into<String>) {
        lock(&self.replies).push_back(text.into());
    }

    /// Every request received so far, oldest first.
    pub async fn requests(&self) -> Vec<Request> {
        self.server.received_requests().await.unwrap_or_default()
    }

    /// The `POST /v1/messages` requests received so far, oldest first.
    pub async fn message_requests(&self) -> Vec<Request> {
        self.requests()
            .await
            .into_iter()
            .filter(|request| {
                request.method == wiremock::http::Method::POST
                    && request.url.path() == "/v1/messages"
            })
            .collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Messages {
    replies: Arc<Mutex<VecDeque<String>>>,
}

impl Respond for Messages {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return ResponseTemplate::new(400).set_body_json(json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "the request body is not JSON",
                },
            }));
        };
        let text = lock(&self.replies)
            .pop_front()
            .unwrap_or_else(|| DEFAULT_REPLY.to_owned());
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(crate::claude::DEFAULT_MODEL);
        let id = format!("msg_fake_{}", uuid::Uuid::new_v4().simple());
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            ResponseTemplate::new(200).set_body_raw(sse(&id, model, &text), "text/event-stream")
        } else {
            ResponseTemplate::new(200).set_body_json(message(&id, model, &text))
        }
    }
}

const INPUT_TOKENS: u64 = 10;

fn output_tokens(text: &str) -> u64 {
    u64::try_from(text.split_whitespace().count())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn message(id: &str, model: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": INPUT_TOKENS, "output_tokens": output_tokens(text)},
    })
}

fn sse(id: &str, model: &str, text: &str) -> String {
    let events = [
        json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": INPUT_TOKENS, "output_tokens": 1},
            },
        }),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": text},
        }),
        json!({"type": "content_block_stop", "index": 0}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": output_tokens(text)},
        }),
        json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn events(body: &str) -> Vec<(String, Value)> {
        body.split("\n\n")
            .filter(|block| !block.is_empty())
            .map(|block| {
                let (event, data) = block.split_once('\n').unwrap();
                let name = event.strip_prefix("event: ").unwrap().to_owned();
                let data: Value =
                    serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
                assert_eq!(data["type"], name.as_str());
                (name, data)
            })
            .collect()
    }

    #[tokio::test]
    async fn streams_the_queued_reply_as_sse() {
        let fake = fake_anthropic().await;
        fake.push_reply("scripted reply");
        let response = client()
            .post(format!("{}/v1/messages?beta=true", fake.uri()))
            .header("authorization", "Bearer placeholder")
            .body(r#"{"model":"claude-test","stream":true,"messages":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let events = events(&response.text().await.unwrap());
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[0].1["message"]["model"], "claude-test");
        assert_eq!(events[2].1["delta"]["type"], "text_delta");
        assert_eq!(events[2].1["delta"]["text"], "scripted reply");
        assert_eq!(events[4].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[4].1["usage"]["output_tokens"], 2);
    }

    #[tokio::test]
    async fn answers_json_without_stream_then_falls_back_to_the_default_reply() {
        let fake = fake_anthropic().await;
        fake.push_reply("first");
        for (stream, expected) in [("false", "first"), ("null", DEFAULT_REPLY)] {
            let body: Value = client()
                .post(format!("{}/v1/messages", fake.uri()))
                .body(format!(r#"{{"stream":{stream}}}"#))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(body["type"], "message");
            assert_eq!(body["model"], crate::claude::DEFAULT_MODEL);
            assert_eq!(body["content"][0]["text"], expected);
            assert_eq!(body["stop_reason"], "end_turn");
        }
    }

    #[tokio::test]
    async fn answers_hello_refuses_bad_bodies_and_records_everything() {
        let fake = fake_anthropic().await;
        let hello = client()
            .head(format!("{}/api/hello", fake.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(hello.status(), 200);
        let bad = client()
            .post(format!("{}/v1/messages", fake.uri()))
            .header("x-api-key", "key-placeholder")
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
        let err: Value = bad.json().await.unwrap();
        assert_eq!(err["error"]["type"], "invalid_request_error");
        let other = client()
            .get(format!("{}/v1/models", fake.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(other.status(), 404);

        let requests = fake.requests().await;
        let paths: Vec<&str> = requests.iter().map(|r| r.url.path()).collect();
        assert_eq!(paths, ["/api/hello", "/v1/messages", "/v1/models"]);
        let messages = fake.message_requests().await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].headers["x-api-key"], "key-placeholder");
        assert_eq!(messages[0].body, b"not json");
    }

    #[tokio::test]
    async fn serves_on_a_given_listener_with_registered_mocks() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let fake = FakeAnthropic::start_on(listener).await;
        assert_eq!(fake.uri(), format!("http://{address}"));
        fake.register(
            Mock::given(method("GET"))
                .and(path("/api/oauth/profile"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true}))),
        )
        .await;
        let profile = client()
            .get(format!("{}/api/oauth/profile", fake.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(profile.status(), 200);
        let hello = client()
            .head(format!("{}/api/hello", fake.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(hello.status(), 200);
        let other = client()
            .get(format!("{}/v1/models", fake.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(other.status(), 404);
    }
}
