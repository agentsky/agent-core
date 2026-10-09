//! The public listener's [`RefuseSubnet`] guard.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::serve::Listener;
use core_types::{Cidr, Throttle};
use tokio::net::{TcpListener, TcpStream};

/// How often [`RefuseSubnet`] logs a warning for one peer address. Further
/// refusals from it within this window are logged at debug level, and
/// counted in its next warning. While the [`Throttle`] is full of recently
/// warned addresses
/// ([`MAX_THROTTLE_KEYS`](core_types::throttle::MAX_THROTTLE_KEYS)), a new
/// address's refusals are all logged at debug level, uncounted.
pub const REFUSAL_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// A TCP listener that drops every connection from one subnet as soon as it
/// is accepted, before a byte is read.
///
/// The public listener uses it to refuse the sandbox subnet, as a second
/// guard behind binding to the `egress` address only. A refusal is logged
/// as a warning at most once per peer address every
/// [`REFUSAL_WARN_INTERVAL`], so a sandbox that keeps retrying can't flood
/// the log.
#[derive(Debug)]
pub struct RefuseSubnet {
    inner: TcpListener,
    refused: Cidr,
    log: Throttle<IpAddr>,
}

impl RefuseSubnet {
    /// Wraps `inner`, refusing peers in `refused`.
    pub fn new(inner: TcpListener, refused: Cidr) -> Self {
        Self {
            inner,
            refused,
            log: Throttle::new(REFUSAL_WARN_INTERVAL),
        }
    }
}

impl Listener for RefuseSubnet {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        loop {
            let (io, peer) = Listener::accept(&mut self.inner).await;
            if !self.refused.contains(peer.ip()) {
                return (io, peer);
            }
            match self.log.record(peer.ip(), Instant::now()) {
                Some(quiet) => tracing::warn!(
                    %peer,
                    subnet = %self.refused,
                    refused_since_last_warning = quiet,
                    "refused a connection from the sandbox subnet"
                ),
                None => tracing::debug!(
                    %peer,
                    subnet = %self.refused,
                    "refused a connection from the sandbox subnet"
                ),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn cidr(s: &str) -> Cidr {
        s.parse().unwrap()
    }

    async fn accept_one(listener: &mut RefuseSubnet) -> Option<SocketAddr> {
        tokio::time::timeout(std::time::Duration::from_millis(300), listener.accept())
            .await
            .ok()
            .map(|(_, peer)| peer)
    }

    #[tokio::test]
    async fn refuses_peers_in_the_subnet_and_accepts_others() {
        let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = inner.local_addr().unwrap();
        let mut refusing = RefuseSubnet::new(inner, cidr("127.0.0.0/8"));
        assert_eq!(refusing.local_addr().unwrap(), addr);

        let mut client = TcpStream::connect(addr).await.unwrap();
        assert_eq!(accept_one(&mut refusing).await, None);
        client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.ok();
        let mut buf = Vec::new();
        let read = client.read_to_end(&mut buf).await;
        assert!(read.is_err() || buf.is_empty(), "{read:?} {buf:?}");

        let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = inner.local_addr().unwrap();
        let mut accepting = RefuseSubnet::new(inner, cidr("172.30.0.0/24"));
        let client = TcpStream::connect(addr).await.unwrap();
        assert_eq!(
            accept_one(&mut accepting).await,
            Some(client.local_addr().unwrap())
        );
    }
}
