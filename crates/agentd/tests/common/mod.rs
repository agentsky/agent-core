#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use secrecy::ExposeSecret;

pub const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:0"
drain_timeout_secs = 5

[internal]
proxy_listen = "127.0.0.2:0"
ctl_listen = "127.0.0.2:0"
sandbox_subnet = "127.0.0.2/32"

[store]
url = "sqlite::memory:"
data_dir = "/nonexistent/agentd"
"#;

pub fn master_key() -> String {
    store::Sealer::generate_key()
        .unwrap()
        .expose_secret()
        .to_owned()
}

pub fn env() -> Vec<(String, String)> {
    vec![("AGENTD_MASTER_KEY".to_owned(), master_key())]
}

/// A response: the status code, and the body if the server sent one.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Sends `GET path` over a new connection and reads the whole response.
/// Returns `None` if the connection is closed before a status line arrives.
pub fn get(addr: SocketAddr, path: &str) -> Option<Response> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: agentd\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    read_response(&mut stream)
}

/// Sends `POST path` with `headers` and `body` over a new connection and
/// reads the whole response. Returns `None` if the connection is closed
/// before a status line arrives.
pub fn post(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, String)],
    body: &str,
) -> Option<Response> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: agentd\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).ok()?;
    read_response(&mut stream)
}

pub fn read_response(stream: &mut TcpStream) -> Option<Response> {
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let raw = String::from_utf8(raw).ok()?;
    let status = raw.split(' ').nth(1)?.parse().ok()?;
    let body = raw
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body)
        .to_owned();
    Some(Response { status, body })
}

/// A directory under the system temp directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "agentd-test-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
