//! [`FakeProxy`]: a proxy that answers nothing and records what reached it,
//! for tests that a client goes around, or through, a proxy it was given;
//! and [`assert_loopback_skips_proxy`], the test every client that calls a
//! configured URL runs.

use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A listener on a loopback port that reads the start of each connection,
/// records its first line (`POST http://… HTTP/1.1` for a plain request,
/// `CONNECT host:443 HTTP/1.1` for a tunnel), and hangs up without an
/// answer. Give a client `reqwest::Proxy::all(proxy.url())` as if the
/// system had that proxy, so no test sets an environment variable.
pub struct FakeProxy {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl FakeProxy {
    /// Starts the proxy.
    ///
    /// # Panics
    ///
    /// If no loopback port can be bound.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut head = Vec::new();
                let mut chunk = [0; 1024];
                while !head.contains(&b'\n') && head.len() < 8192 {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&chunk[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                record
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(line);
            }
        });
        Self { url, seen, task }
    }

    /// The proxy's URL, `http://127.0.0.1:<port>`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The first line of every connection that reached the proxy so far.
    pub fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for FakeProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Checks that a client goes straight to a loopback address it was
/// configured with, though the system has a proxy, and that it does use the
/// proxy otherwise, so the first check proves something.
///
/// `build` builds the client under test for a configured base URL, given a
/// proxy to add as if the system had it. It is called once with a fake
/// server's `http://127.0.0.1:<port>` and once with
/// `https://api.example.com`; both clients then send a `GET` to the fake
/// server.
///
/// # Panics
///
/// If the loopback client used the proxy or failed, or the other one did
/// not reach the proxy.
pub async fn assert_loopback_skips_proxy(build: impl Fn(&str, reqwest::Proxy) -> reqwest::Client) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let proxy = FakeProxy::start().await;
    let system = || reqwest::Proxy::all(proxy.url()).expect("a proxy URL");
    let target = format!("{}/probe", server.uri());

    let direct = build(&server.uri(), system())
        .get(&target)
        .send()
        .await
        .expect("the loopback request is answered");
    assert_eq!(direct.status(), 204);
    assert_eq!(
        proxy.seen(),
        Vec::<String>::new(),
        "the proxy saw a loopback request"
    );

    let proxied = build("https://api.example.com", system())
        .get(&target)
        .send()
        .await;
    assert!(proxied.is_err(), "the proxy answers nothing");
    assert_eq!(proxy.seen(), [format!("GET {target} HTTP/1.1")]);
}
