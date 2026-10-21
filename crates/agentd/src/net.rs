//! Network helpers: [`Cidr`] subnets and the public listener's
//! [`RefuseSubnet`] guard.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::time::{Duration, Instant};

use axum::serve::Listener;
use serde::Deserialize;
use tokio::net::{TcpListener, TcpStream};

/// An IP subnet in CIDR form, such as `172.30.0.0/24`.
///
/// The address must be the network address: `172.30.0.5/24` is refused
/// rather than silently widened, since it usually means a typo.
///
/// An IPv4 subnet written in IPv4-mapped IPv6 form, such as
/// `::ffff:172.30.0.0/120`, is stored as the IPv4 subnet it names
/// (`172.30.0.0/24`), so it matches IPv4 peers however they are reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

/// Why a string isn't a valid [`Cidr`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CidrError {
    /// There is no `/prefix` part.
    #[error("expected an address and a prefix length, such as 172.30.0.0/24")]
    Format,
    /// The address part isn't an IPv4 or IPv6 address.
    #[error("the address is not an IPv4 or IPv6 address")]
    Address,
    /// The prefix length isn't a number, or is longer than the address.
    #[error("the prefix length must be a number from 0 to {max}")]
    Prefix {
        /// 32 for IPv4, 128 for IPv6.
        max: u8,
    },
    /// Bits after the prefix are set.
    #[error("the address has bits set after the prefix; the network is {0}")]
    HostBits(Cidr),
}

impl Cidr {
    /// The subnet `network/prefix`. An IPv4-mapped `network` with a prefix
    /// of at least 96 becomes the IPv4 subnet it names.
    ///
    /// # Errors
    ///
    /// [`CidrError::Prefix`] if `prefix` is longer than the address,
    /// [`CidrError::HostBits`] if `network` has bits set after the prefix.
    pub fn new(network: IpAddr, prefix: u8) -> Result<Self, CidrError> {
        let max = match network {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix > max {
            return Err(CidrError::Prefix { max });
        }
        let (network, prefix) = match network {
            IpAddr::V6(v6) if prefix >= 96 => match v6.to_ipv4_mapped() {
                Some(v4) => (IpAddr::V4(v4), prefix - 96),
                None => (network, prefix),
            },
            _ => (network, prefix),
        };
        let masked = mask(network, prefix);
        if masked != network {
            return Err(CidrError::HostBits(Self {
                network: masked,
                prefix,
            }));
        }
        Ok(Self { network, prefix })
    }

    /// Whether `ip` is in this subnet. An IPv4-mapped IPv6 address, as a
    /// dual-stack socket reports IPv4 peers, counts as its IPv4 address, and
    /// an IPv4 address is in an IPv6 subnet that holds its mapped form.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match (self.network, ip.to_canonical()) {
            (IpAddr::V6(_), IpAddr::V4(v4)) => IpAddr::V6(v4.to_ipv6_mapped()),
            (IpAddr::V4(_), IpAddr::V6(_)) => return false,
            (_, ip) => ip,
        };
        mask(ip, self.prefix) == self.network
    }
}

fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4((u32::from(v4) & bits).into())
        }
        IpAddr::V6(v6) => {
            let bits = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6((u128::from(v6) & bits).into())
        }
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl FromStr for Cidr {
    type Err = CidrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = s.split_once('/').ok_or(CidrError::Format)?;
        let network: IpAddr = address.parse().map_err(|_| CidrError::Address)?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = if prefix.bytes().all(|b| b.is_ascii_digit()) {
            prefix.parse().map_err(|_| CidrError::Prefix { max })?
        } else {
            return Err(CidrError::Prefix { max });
        };
        Self::new(network, prefix)
    }
}

impl TryFrom<String> for Cidr {
    type Error = CidrError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

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
    use std::net::{Ipv4Addr, Ipv6Addr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn cidr(s: &str) -> Cidr {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_displays_subnets() {
        for s in [
            "172.30.0.0/24",
            "10.0.0.0/8",
            "0.0.0.0/0",
            "10.1.2.3/32",
            "fd00::/8",
            "::1/128",
        ] {
            assert_eq!(cidr(s).to_string(), s);
        }
    }

    #[test]
    fn refuses_malformed_subnets() {
        assert_eq!("172.30.0.0".parse::<Cidr>(), Err(CidrError::Format));
        assert_eq!("172.30.0/24".parse::<Cidr>(), Err(CidrError::Address));
        assert_eq!(
            "172.30.0.0/33".parse::<Cidr>(),
            Err(CidrError::Prefix { max: 32 })
        );
        assert_eq!(
            "172.30.0.0/+8".parse::<Cidr>(),
            Err(CidrError::Prefix { max: 32 })
        );
        assert_eq!(
            "172.30.0.0/".parse::<Cidr>(),
            Err(CidrError::Prefix { max: 32 })
        );
        assert_eq!(
            "fd00::/129".parse::<Cidr>(),
            Err(CidrError::Prefix { max: 128 })
        );
        assert_eq!(
            "fd00::/999".parse::<Cidr>(),
            Err(CidrError::Prefix { max: 128 })
        );
        let err = "172.30.0.5/24".parse::<Cidr>().unwrap_err();
        assert_eq!(err, CidrError::HostBits(cidr("172.30.0.0/24")));
        assert_eq!(
            err.to_string(),
            "the address has bits set after the prefix; the network is 172.30.0.0/24"
        );
    }

    #[test]
    fn contains_only_addresses_in_the_subnet() {
        let v4 = cidr("172.30.0.0/24");
        assert!(v4.contains(ip("172.30.0.0")));
        assert!(v4.contains(ip("172.30.0.255")));
        assert!(!v4.contains(ip("172.30.1.0")));
        assert!(!v4.contains(ip("172.29.255.255")));
        assert!(v4.contains(ip("::ffff:172.30.0.9")));
        assert!(!v4.contains(ip("fd00::1")));

        let v6 = cidr("fd00:1::/64");
        assert!(v6.contains(ip("fd00:1::abcd")));
        assert!(!v6.contains(ip("fd00:2::1")));
        assert!(!v6.contains(ip("172.30.0.1")));

        assert!(cidr("0.0.0.0/0").contains(Ipv4Addr::BROADCAST.into()));
        assert!(!cidr("0.0.0.0/0").contains(Ipv6Addr::LOCALHOST.into()));
        assert!(cidr("10.1.2.3/32").contains(ip("10.1.2.3")));
        assert!(!cidr("10.1.2.3/32").contains(ip("10.1.2.4")));
    }

    #[test]
    fn an_ipv4_mapped_subnet_is_the_ipv4_subnet_it_names() {
        let mapped = cidr("::ffff:172.30.0.0/120");
        assert_eq!(mapped, cidr("172.30.0.0/24"));
        assert_eq!(mapped.to_string(), "172.30.0.0/24");
        assert!(mapped.contains(ip("172.30.0.9")));
        assert!(mapped.contains(ip("::ffff:172.30.0.9")));
        assert!(!mapped.contains(ip("172.30.1.9")));
        assert_eq!(cidr("::ffff:10.1.2.3/128"), cidr("10.1.2.3/32"));
        assert_eq!(cidr("::ffff:0.0.0.0/96"), cidr("0.0.0.0/0"));
        assert_eq!(
            "::ffff:172.30.0.5/120".parse::<Cidr>(),
            Err(CidrError::HostBits(cidr("172.30.0.0/24")))
        );
    }

    #[test]
    fn an_ipv6_subnet_holding_the_mapped_range_contains_ipv4_peers() {
        let all = cidr("::/0");
        assert!(all.contains(ip("172.30.0.9")));
        assert!(all.contains(ip("::ffff:172.30.0.9")));
        assert!(all.contains(ip("fd00::1")));
        let unrelated = cidr("fd00::/8");
        assert!(!unrelated.contains(ip("172.30.0.9")));
        assert!(!unrelated.contains(ip("::ffff:172.30.0.9")));
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
