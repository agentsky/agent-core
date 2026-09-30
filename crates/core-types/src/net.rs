//! [`Cidr`]: IP subnets, for the listeners' network checks and the
//! egress proxy's address rules.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::Deserialize;

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

    /// The network address.
    pub fn network(&self) -> IpAddr {
        self.network
    }

    /// The prefix length.
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Whether every address of `other` is in this subnet.
    pub fn covers(&self, other: &Cidr) -> bool {
        let other_prefix = match (self.network, other.network) {
            (IpAddr::V6(_), IpAddr::V4(_)) => other.prefix + 96,
            (IpAddr::V4(_), IpAddr::V6(_)) => return false,
            _ => other.prefix,
        };
        self.prefix <= other_prefix && self.contains(other.network)
    }

    /// Whether this subnet and `other` share any address. Two subnets are
    /// either disjoint or one covers the other.
    pub fn overlaps(&self, other: &Cidr) -> bool {
        self.covers(other) || other.covers(self)
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

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

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
    fn covers_and_overlaps_compare_whole_subnets() {
        let wide = cidr("10.0.0.0/8");
        let narrow = cidr("10.1.0.0/16");
        assert!(wide.covers(&narrow));
        assert!(!narrow.covers(&wide));
        assert!(wide.overlaps(&narrow) && narrow.overlaps(&wide));
        assert!(wide.covers(&wide));
        assert!(!wide.overlaps(&cidr("11.0.0.0/8")));
        assert!(!wide.covers(&cidr("fd00::/8")));
        assert!(!cidr("fd00::/8").covers(&wide));
        let mapped_range = cidr("::/0");
        assert!(mapped_range.covers(&wide));
        assert!(!cidr("::ffff:0:0/97").covers(&cidr("0.0.0.0/0")));
        assert!(cidr("::ffff:0:0/96").covers(&cidr("172.30.0.0/24")));
        assert!(cidr("10.1.2.3/32").overlaps(&wide));
        assert_eq!(narrow.network(), ip("10.1.0.0"));
        assert_eq!(narrow.prefix(), 16);
    }
}
