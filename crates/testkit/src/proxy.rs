//! [`FakeProxy`]: a proxy that answers nothing and records what reached it,
//! for tests that a client goes around, or through, a proxy it was given;
//! and [`assert_proxied_only_elsewhere`], the test every client that calls
//! a configured URL runs.

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

/// Checks that a client goes straight to the bases it should reach without
/// a proxy, though the system has one, and that it does use the proxy
/// otherwise, so the first checks prove something.
///
/// `build` builds the client under test for a configured base URL, given a
/// proxy to add as if the system had it. It is called with a fake server's
/// `http://127.0.0.1:<port>`, with each of `also_direct`, and with
/// `https://api.example.com`. Each client then sends a `GET` to the fake
/// server: all but the last must reach it directly, and the last must go
/// to the proxy.
///
/// # Panics
///
/// If a client that should go direct used the proxy or failed, or the last
/// one did not reach the proxy.
pub async fn assert_proxied_only_elsewhere(
    build: impl Fn(&str, reqwest::Proxy) -> reqwest::Client,
    also_direct: &[&str],
) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let proxy = FakeProxy::start().await;
    let system = || reqwest::Proxy::all(proxy.url()).expect("a proxy URL");
    let target = format!("{}/probe", server.uri());

    let direct =
        std::iter::once(server.uri()).chain(also_direct.iter().map(|base| (*base).to_owned()));
    for base in direct {
        let answer = build(&base, system())
            .get(&target)
            .send()
            .await
            .unwrap_or_else(|err| panic!("{base}: the request failed: {err}"));
        assert_eq!(answer.status(), 204, "{base}");
        assert_eq!(
            proxy.seen(),
            Vec::<String>::new(),
            "{base} went through the proxy"
        );
    }

    let proxied = build("https://api.example.com", system())
        .get(&target)
        .send()
        .await;
    assert!(proxied.is_err(), "the proxy answers nothing");
    assert_eq!(proxy.seen(), [format!("GET {target} HTTP/1.1")]);
}
