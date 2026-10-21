//! What the egress proxy lets a sandbox reach: [`HostRule`]s naming hosts,
//! and an [`EgressPolicy`] that also decides which resolved addresses are
//! reachable.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use core_types::Cidr;
use serde::Deserialize;

/// The host every sandbox is denied, whatever the allowlist says: the CLI
/// reaches it only through `ANTHROPIC_BASE_URL`, so side traffic that
/// bypasses the credential proxy fails loudly.
pub const ANTHROPIC_API_HOST: &str = "api.anthropic.com";

/// The port a rule without one allows.
pub const DEFAULT_PORT: u16 = 443;

/// The longest host name DNS allows, without the trailing dot.
const MAX_HOST_LEN: usize = 253;

/// The longest DNS label.
const MAX_LABEL_LEN: usize = 63;

/// One allowlist entry: an exact host, or `*.` and a suffix matching every
/// name below it (at any depth, but not the suffix itself), with an
/// optional `:port`. Without a port it allows port 443 only; with one, only
/// that port.
///
/// Hosts are DNS names of letters, digits and `-`, compared ignoring case
/// and one trailing dot. IP addresses are not hosts: the egress proxy never
/// connects to an address the client names. Every host, a wildcard's
/// suffix included, has at least two labels, so `*.com` is refused.
/// [`ANTHROPIC_API_HOST`] is refused too, on any port, since the egress
/// proxy always denies it; a wildcard above it, such as `*.anthropic.com`,
/// is accepted and still never reaches it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct HostRule {
    host: String,
    wildcard: bool,
    port: u16,
}

/// Why a string isn't a valid [`HostRule`]. It never repeats the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HostRuleError(&'static str);

impl HostRule {
    /// Whether the rule allows `host` (already [normalized](normalize_host))
    /// on `port`.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        port == self.port && self.names(host)
    }

    /// Whether the rule names `host`, on any port.
    pub(crate) fn names(&self, host: &str) -> bool {
        if self.wildcard {
            host.strip_suffix(self.host.as_str())
                .and_then(|rest| rest.strip_suffix('.'))
                .is_some_and(|rest| !rest.is_empty())
        } else {
            host == self.host
        }
    }
}

impl FromStr for HostRule {
    type Err = HostRuleError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (name, port) = match text.rsplit_once(':') {
            Some((name, port)) => (
                name,
                parse_port(port)
                    .ok_or(HostRuleError("the port must be a number from 1 to 65535"))?,
            ),
            None => (text, DEFAULT_PORT),
        };
        let (wildcard, name) = match name.strip_prefix("*.") {
            Some(suffix) => (true, suffix),
            None => (false, name),
        };
        let host = normalize_host(name).ok_or(HostRuleError(
            "expected a host name of two or more labels, such as github.com or \
             *.example.com, with an optional :port; IP addresses are not allowed",
        ))?;
        if !wildcard && host == ANTHROPIC_API_HOST {
            return Err(HostRuleError(
                "api.anthropic.com is always denied; sandboxes reach it through the \
                 credential proxy",
            ));
        }
        Ok(Self {
            host,
            wildcard,
            port,
        })
    }
}

impl TryFrom<String> for HostRule {
    type Error = HostRuleError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl fmt::Display for HostRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.wildcard {
            f.write_str("*.")?;
        }
        f.write_str(&self.host)?;
        if self.port != DEFAULT_PORT {
            write!(f, ":{}", self.port)?;
        }
        Ok(())
    }
}

/// A port in decimal, 1 to 65535.
pub(crate) fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|&port| port != 0)
}

/// `name` in the one form hosts are compared in: lowercase, without a
/// trailing dot. `None` unless it is a DNS host name: labels of letters,
/// digits and `-` (not first or last), at most 63 bytes each and 253 in
/// all, at least two of them, the last starting with a letter.
///
/// Requiring a letter to start the last label refuses every form a
/// resolver could read as an IPv4 address (`127.1`, `2130706433`,
/// `0x7f.1`), as well as IPv6 literals, which hold `:` or `[`.
pub fn normalize_host(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > MAX_HOST_LEN {
        return None;
    }
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |label: &&str| {
        !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let last_ok = labels
        .last()
        .and_then(|last| last.bytes().next())
        .is_some_and(|first| first.is_ascii_alphabetic());
    (labels.len() >= 2 && labels.iter().all(label_ok) && last_ok).then(|| name.to_ascii_lowercase())
}

/// What a sandbox may reach through the egress proxy.
///
/// A host must match a [`HostRule`], either the configured ones or the
/// ones an [`EgressExtension`](crate::EgressExtension) adds for the
/// session's agent, and every address it resolves to must be reachable.
/// [`ANTHROPIC_API_HOST`] is always denied. These addresses are never
/// reachable, whichever rule allowed the host:
///
/// - agentd's own networks, as given to [`new`](Self::new): its addresses
///   and the sandbox network;
/// - private addresses: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`
///   and `fc00::/7`;
/// - cloud metadata and platform addresses: link-local `169.254.0.0/16`
///   (`169.254.169.254` among them) and `fe80::/10`, AWS's
///   `fd00:ec2::254`, GCP's `fd20:ce::254`, Oracle's
///   `fd00:c1::a9fe:a9fe`, Azure's WireServer `168.63.129.16`, Alibaba's
///   `100.100.100.200` (in `100.64.0.0/10`) and Oracle's `192.0.0.192`
///   (in `192.0.0.0/24`);
/// - loopback, `0.0.0.0/8`, shared address space (`100.64.0.0/10`),
///   `192.0.0.0/24`, documentation and benchmarking ranges, multicast,
///   reserved and broadcast addresses;
/// - IPv6 outside global unicast (`2000::/3`), and the parts of it that
///   embed or relay IPv4 or aren't routed: `2001::/23` (Teredo among them),
///   `2002::/16` (6to4), `2001:db8::/32` and `3fff::/20`.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    rules: Vec<HostRule>,
    own: Vec<Cidr>,
}

/// The reason logged for an address in one of agentd's own networks.
pub(crate) const OWN_NETWORK: &str = "agentd's own network";

impl EgressPolicy {
    /// A policy allowing `rules`, and never reaching `own`: agentd's
    /// addresses and the sandbox network.
    pub fn new(rules: Vec<HostRule>, own: Vec<Cidr>) -> Self {
        Self { rules, own }
    }

    /// The configured rules.
    pub fn rules(&self) -> &[HostRule] {
        &self.rules
    }

    /// Why `ip` is unreachable, for the log line, or `None` if it is
    /// reachable.
    pub(crate) fn unreachable(&self, ip: IpAddr) -> Option<&'static str> {
        let ip = ip.to_canonical();
        if self.own.iter().any(|net| net.contains(ip)) {
            return Some(OWN_NETWORK);
        }
        match ip {
            IpAddr::V4(v4) => SPECIAL_V4
                .iter()
                .find(|(network, prefix, _)| within_v4(v4, *network, *prefix))
                .map(|(_, _, kind)| *kind),
            IpAddr::V6(v6) => unreachable_v6(v6),
        }
    }
}

/// IPv4 ranges that are never reachable, with what they are. The first
/// match names the address.
const SPECIAL_V4: [([u8; 4], u8, &str); 16] = [
    ([0, 0, 0, 0], 8, "\"this network\" address"),
    ([127, 0, 0, 0], 8, "loopback address"),
    ([169, 254, 0, 0], 16, "link-local or cloud metadata address"),
    ([168, 63, 129, 16], 32, "cloud metadata address"),
    ([100, 64, 0, 0], 10, "shared address space"),
    ([192, 0, 0, 0], 24, "IETF protocol address"),
    ([192, 0, 2, 0], 24, "documentation address"),
    ([198, 51, 100, 0], 24, "documentation address"),
    ([203, 0, 113, 0], 24, "documentation address"),
    ([198, 18, 0, 0], 15, "benchmarking address"),
    ([192, 88, 99, 0], 24, "6to4 relay address"),
    ([224, 0, 0, 0], 4, "multicast address"),
    ([240, 0, 0, 0], 4, "reserved or broadcast address"),
    ([10, 0, 0, 0], 8, PRIVATE),
    ([172, 16, 0, 0], 12, PRIVATE),
    ([192, 168, 0, 0], 16, PRIVATE),
];

/// Cloud metadata addresses inside the IPv6 private range: AWS's, GCP's
/// and Oracle's.
const METADATA_V6: [Ipv6Addr; 3] = [
    Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254),
    Ipv6Addr::new(0xfd20, 0xce, 0, 0, 0, 0, 0, 0x254),
    Ipv6Addr::new(0xfd00, 0xc1, 0, 0, 0, 0, 0xa9fe, 0xa9fe),
];

/// IPv6 ranges inside `2000::/3` that are never reachable.
const SPECIAL_V6: [(u128, u8, &str); 4] = [
    (0x2001_0000 << 96, 23, "IETF protocol or Teredo address"),
    (0x2002 << 112, 16, "6to4 address"),
    (0x2001_0db8 << 96, 32, "documentation address"),
    (0x3fff << 112, 20, "documentation address"),
];

const PRIVATE: &str = "private address";

fn within_v4(ip: Ipv4Addr, network: [u8; 4], prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(network)) & mask
}

fn within_v6(ip: Ipv6Addr, network: u128, prefix: u8) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
    u128::from(ip) & mask == network & mask
}

fn unreachable_v6(v6: Ipv6Addr) -> Option<&'static str> {
    if METADATA_V6.contains(&v6) {
        Some("cloud metadata address")
    } else if within_v6(v6, 0xfc00 << 112, 7) {
        Some(PRIVATE)
    } else if !within_v6(v6, 0x2000 << 112, 3) {
        Some("IPv6 address outside global unicast")
    } else {
        SPECIAL_V6
            .iter()
            .find(|(network, prefix, _)| within_v6(v6, *network, *prefix))
            .map(|(_, _, kind)| *kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(text: &str) -> HostRule {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn cidr(text: &str) -> Cidr {
        text.parse().unwrap()
    }

    #[test]
    fn exact_rules_match_one_host_on_443_unless_they_name_a_port() {
        let github = rule("GitHub.com.");
        assert_eq!(github.to_string(), "github.com");
        assert!(github.allows("github.com", 443));
        assert!(!github.allows("github.com", 80));
        assert!(!github.allows("api.github.com", 443));
        assert!(!github.allows("evilgithub.com", 443));
        let ported = rule("git.example.com:8443");
        assert_eq!(ported.to_string(), "git.example.com:8443");
        assert!(ported.allows("git.example.com", 8443));
        assert!(!ported.allows("git.example.com", 443));
        assert_eq!(rule("git.example.com:443").to_string(), "git.example.com");
    }

    #[test]
    fn wildcard_rules_match_names_below_the_suffix_only() {
        let wild = rule("*.githubusercontent.com");
        assert_eq!(wild.to_string(), "*.githubusercontent.com");
        assert!(wild.allows("raw.githubusercontent.com", 443));
        assert!(wild.allows("a.b.githubusercontent.com", 443));
        assert!(!wild.allows("githubusercontent.com", 443));
        assert!(!wild.allows("evilgithubusercontent.com", 443));
        assert!(!wild.allows("raw.githubusercontent.com", 22));
        assert!(rule("*.example.com:22").allows("git.example.com", 22));
    }

    #[test]
    fn rules_that_are_not_host_names_are_refused() {
        for text in [
            "",
            "*",
            "*.com",
            "com",
            "localhost",
            "a.*.example.com",
            "*example.com",
            "**.example.com",
            "1.2.3.4",
            "127.1",
            "2130706433",
            "0x7f.1",
            "[::1]",
            "::1",
            "fd00::1",
            "example.com:0",
            "example.com:65536",
            "example.com:",
            "example.com:+1",
            "example.com:1:2",
            "user@example.com",
            "exa mple.com",
            "exa_mple.com",
            "-example.com",
            "example-.com",
            "example..com",
            "ex%61mple.com",
            "bücher.example",
            "example.com/path",
        ] {
            assert!(text.parse::<HostRule>().is_err(), "{text:?} parsed");
        }
        let long_label = format!("{}.com", "a".repeat(64));
        assert!(long_label.parse::<HostRule>().is_err());
        let long_name = format!("{}com", "a.".repeat(127));
        assert!(long_name.len() > 253);
        assert!(long_name.parse::<HostRule>().is_err());
        assert!("xn--bcher-kva.example".parse::<HostRule>().is_ok());
        let err = "1.2.3.4".parse::<HostRule>().unwrap_err().to_string();
        assert!(err.contains("IP addresses are not allowed"), "{err}");
    }

    #[test]
    fn host_names_are_normalized_before_matching() {
        assert_eq!(
            normalize_host("API.Anthropic.COM.").as_deref(),
            Some(ANTHROPIC_API_HOST)
        );
        assert_eq!(normalize_host("example.com.."), None);
        assert_eq!(normalize_host("."), None);
        assert_eq!(normalize_host("a.b-c.d0"), Some("a.b-c.d0".to_owned()));
        assert_eq!(normalize_host("a.0d"), None);
    }

    fn policy() -> EgressPolicy {
        EgressPolicy::new(
            vec![rule("example.com")],
            vec![cidr("172.30.0.0/24"), cidr("172.31.0.2/32")],
        )
    }

    fn reason(text: &str) -> Option<&'static str> {
        policy().unreachable(ip(text))
    }

    #[test]
    fn the_anthropic_api_host_is_not_a_rule() {
        for text in [
            "api.anthropic.com",
            "API.Anthropic.COM.",
            "api.anthropic.com:443",
            "api.anthropic.com:8443",
        ] {
            let err = text.parse::<HostRule>().unwrap_err().to_string();
            assert!(err.contains("always denied"), "{text}: {err}");
        }
        assert!(rule("*.anthropic.com").names("console.anthropic.com"));
        assert!(rule("*.api.anthropic.com").names("x.api.anthropic.com"));
    }

    #[test]
    fn link_local_metadata_loopback_and_special_addresses_are_never_reached() {
        for text in [
            "169.254.169.254",
            "169.254.0.1",
            "::ffff:169.254.169.254",
            "127.0.0.1",
            "127.255.255.254",
            "::1",
            "::",
            "0.0.0.0",
            "0.1.2.3",
            "100.64.0.1",
            "100.100.100.200",
            "192.0.0.192",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "198.18.0.1",
            "198.19.255.255",
            "192.88.99.1",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "64:ff9b::a9fe:a9fe",
            "::a9fe:a9fe",
            "100::1",
            "2001::1",
            "2001:0:4136:e378::1",
            "2002:a9fe:a9fe::1",
            "2001:db8::1",
            "3fff::1",
        ] {
            let why = reason(text);
            assert!(
                why.is_some_and(|why| why != PRIVATE && why != OWN_NETWORK),
                "{text}: {why:?}"
            );
        }
    }

    #[test]
    fn every_cloud_metadata_address_is_never_reached() {
        for text in [
            "fd00:ec2::254",
            "fd20:ce::254",
            "fd00:c1::a9fe:a9fe",
            "168.63.129.16",
            "::ffff:168.63.129.16",
        ] {
            assert_eq!(reason(text), Some("cloud metadata address"), "{text}");
        }
        assert_eq!(
            reason("169.254.169.254"),
            Some("link-local or cloud metadata address")
        );
        assert_eq!(reason("100.100.100.200"), Some("shared address space"));
        assert_eq!(reason("192.0.0.192"), Some("IETF protocol address"));
        for text in ["168.63.129.15", "168.63.129.17"] {
            assert_eq!(reason(text), None, "{text}");
        }
    }

    #[test]
    fn private_ranges_are_always_denied() {
        for text in [
            "10.0.0.1",
            "10.20.9.9",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "fc00::1",
            "fd00::1",
            "fd12:3456::9",
            "::ffff:10.0.0.1",
        ] {
            assert_eq!(reason(text), Some(PRIVATE), "{text}");
        }
    }

    #[test]
    fn agentd_and_the_sandbox_network_are_never_reached() {
        for text in [
            "172.30.0.2",
            "172.30.0.200",
            "::ffff:172.30.0.9",
            "172.31.0.2",
        ] {
            assert_eq!(reason(text), Some(OWN_NETWORK), "{text}");
        }
        assert_eq!(reason("172.31.0.3"), Some(PRIVATE));
    }

    #[test]
    fn public_addresses_are_reachable() {
        for text in [
            "140.82.112.3",
            "1.1.1.1",
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "2606:4700::1111",
            "2a00:1450:4001::200e",
            "2001:200::1",
            "::ffff:1.1.1.1",
        ] {
            assert_eq!(reason(text), None, "{text}");
        }
        assert_eq!(policy().rules(), [rule("example.com")]);
    }
}
