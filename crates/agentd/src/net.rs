//! The public listener's [`RefuseSubnet`] guard.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::serve::Listener;
use core_types::Cidr;
use tokio::net::{TcpListener, TcpStream};

/// How often [`RefuseSubnet`] logs a warning for one peer address. Further
/// refusals from it within this window are logged at debug level, and
/// counted in its next warning.
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
    log: RefusalLog,
}

impl RefuseSubnet {
    /// Wraps `inner`, refusing peers in `refused`.
    pub fn new(inner: TcpListener, refused: Cidr) -> Self {
        Self {
            inner,
            refused,
            log: RefusalLog::new(REFUSAL_WARN_INTERVAL),
        }
    }
}

/// Decides which refusals are worth a warning: the first from a peer
/// address, then the first after each `interval`.
#[derive(Debug)]
struct RefusalLog {
    interval: Duration,
    /// For each peer warned about within `interval`: when, and how many of
    /// its refusals have been logged at debug level since.
    peers: HashMap<IpAddr, (Instant, u64)>,
}

impl RefusalLog {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            peers: HashMap::new(),
        }
    }

    /// Records a refusal from `ip` at `now`. Returns how many refusals from
    /// it went without a warning since the last one if this one deserves a
    /// warning, and `None` if it doesn't.
    fn record(&mut self, ip: IpAddr, now: Instant) -> Option<u64> {
        if let Some((warned, quiet)) = self.peers.get_mut(&ip)
            && now.duration_since(*warned) < self.interval
        {
            *quiet += 1;
            return None;
        }
        let quiet = self.peers.remove(&ip).map_or(0, |(_, quiet)| quiet);
        self.peers
            .retain(|_, (warned, _)| now.duration_since(*warned) < self.interval);
        self.peers.insert(ip, (now, 0));
        Some(quiet)
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

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn refusals_warn_once_per_peer_per_interval() {
        let interval = Duration::from_secs(60);
        let mut log = RefusalLog::new(interval);
        let (a, b) = (ip("172.30.0.2"), ip("172.30.0.3"));
        let start = Instant::now();
        assert_eq!(log.record(a, start), Some(0));
        assert_eq!(log.record(a, start + Duration::from_secs(1)), None);
        assert_eq!(log.record(a, start + Duration::from_secs(59)), None);
        assert_eq!(log.record(b, start + Duration::from_secs(2)), Some(0));
        assert_eq!(log.record(b, start + Duration::from_secs(3)), None);
        assert_eq!(log.record(a, start + interval), Some(2));
        assert_eq!(log.record(a, start + interval), None);
        assert_eq!(log.record(b, start + Duration::from_secs(200)), Some(1));
        assert_eq!(
            log.peers.len(),
            1,
            "peers warned about long ago are dropped"
        );
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
