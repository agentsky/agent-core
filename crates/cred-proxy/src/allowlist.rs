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

/// The error returned by [`EgressPolicy::new`] for an `allow_private`
/// entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("entry {index}: {reason}")]
pub struct PolicyError {
    /// The entry's position in `allow_private`.
    pub index: usize,
    /// What is wrong with it.
    pub reason: &'static str,
}

/// What a sandbox may reach through the egress proxy.
///
/// A host must match a [`HostRule`], either the configured ones or the
/// ones an [`EgressExtension`](crate::EgressExtension) adds for the
/// session's agent, and every address it resolves to must be reachable.
/// [`ANTHROPIC_API_HOST`] is always denied. These addresses are never
/// reachable:
///
/// - agentd's own networks, as given to [`new`](Self::new): its addresses
///   and the sandbox network;
/// - loopback, link-local (`169.254.0.0/16`, which holds the cloud
///   metadata address `169.254.169.254`, and `fe80::/10`), `fd00:ec2::254`,
///   `0.0.0.0/8`, shared address space (`100.64.0.0/10`),
///   `192.0.0.0/24`, documentation and benchmarking ranges, multicast,
///   reserved and broadcast addresses;
/// - IPv6 outside global unicast (`2000::/3`), and the parts of it that
///   embed or relay IPv4 or aren't routed: `2001::/23` (Teredo among them),
///   `2002::/16` (6to4), `2001:db8::/32` and `3fff::/20`.
///
/// Private addresses (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`,
/// `fc00::/7`) are denied unless an `allow_private` subnet holds them.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    rules: Vec<HostRule>,
    allow_private: Vec<Cidr>,
    own: Vec<Cidr>,
}

/// Why an address is unreachable, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unreachable {
    Own,
    Special(&'static str),
    Private,
}

impl Unreachable {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Own => "agentd's own network",
            Self::Special(kind) => kind,
            Self::Private => "private address",
        }
    }
}

impl EgressPolicy {
    /// A policy allowing `rules`, and the private addresses in
    /// `allow_private`, and never reaching `own`: agentd's addresses and
    /// the sandbox network.
    ///
    /// # Errors
    ///
    /// [`PolicyError`] if an `allow_private` subnet isn't inside a private
    /// range, holds `fd00:ec2::254`, or overlaps one of `own`.
    pub fn new(
        rules: Vec<HostRule>,
        allow_private: Vec<Cidr>,
        own: Vec<Cidr>,
    ) -> Result<Self, PolicyError> {
        for (index, subnet) in allow_private.iter().enumerate() {
            let fail = |reason| Err(PolicyError { index, reason });
            if !is_private_subnet(subnet) {
                return fail(
                    "must be inside 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 or fc00::/7",
                );
            }
            if subnet.contains(IpAddr::V6(AWS_METADATA_V6)) {
                return fail("must not hold the cloud metadata address fd00:ec2::254");
            }
            if own.iter().any(|net| net.overlaps(subnet)) {
                return fail("must not overlap agentd's addresses or the sandbox network");
            }
        }
        Ok(Self {
            rules,
            allow_private,
            own,
        })
    }

    /// The configured rules.
    pub fn rules(&self) -> &[HostRule] {
        &self.rules
    }

    /// Why `ip` is unreachable, or `None` if it is reachable.
    pub(crate) fn unreachable(&self, ip: IpAddr) -> Option<Unreachable> {
        let ip = ip.to_canonical();
        if self.own.iter().any(|net| net.contains(ip)) {
            return Some(Unreachable::Own);
        }
        match classify(ip) {
            Class::Public => None,
            Class::Special(kind) => Some(Unreachable::Special(kind)),
            Class::Private if self.allow_private.iter().any(|net| net.contains(ip)) => None,
            Class::Private => Some(Unreachable::Private),
        }
    }
}

/// AWS's IPv6 instance metadata address.
const AWS_METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

enum Class {
    Public,
    Private,
    Special(&'static str),
}

/// IPv4 ranges that are never reachable, with what they are.
const SPECIAL_V4: [([u8; 4], u8, &str); 12] = [
    ([0, 0, 0, 0], 8, "\"this network\" address"),
    ([127, 0, 0, 0], 8, "loopback address"),
    ([169, 254, 0, 0], 16, "link-local or cloud metadata address"),
    ([100, 64, 0, 0], 10, "shared address space"),
    ([192, 0, 0, 0], 24, "IETF protocol address"),
    ([192, 0, 2, 0], 24, "documentation address"),
    ([198, 51, 100, 0], 24, "documentation address"),
    ([203, 0, 113, 0], 24, "documentation address"),
    ([198, 18, 0, 0], 15, "benchmarking address"),
    ([192, 88, 99, 0], 24, "6to4 relay address"),
    ([224, 0, 0, 0], 4, "multicast address"),
    ([240, 0, 0, 0], 4, "reserved or broadcast address"),
];

/// IPv6 ranges inside `2000::/3` that are never reachable.
const SPECIAL_V6: [(u128, u8, &str); 4] = [
    (0x2001_0000 << 96, 23, "IETF protocol or Teredo address"),
    (0x2002 << 112, 16, "6to4 address"),
    (0x2001_0db8 << 96, 32, "documentation address"),
    (0x3fff << 112, 20, "documentation address"),
];

/// The IPv4 private ranges, which `allow_private` may open.
const PRIVATE_V4: [([u8; 4], u8); 3] = [
    ([10, 0, 0, 0], 8),
    ([172, 16, 0, 0], 12),
    ([192, 168, 0, 0], 16),
];

/// The IPv6 private range (unique local addresses).
const PRIVATE_V6: (u128, u8) = (0xfc00 << 112, 7);

/// Whether all of `subnet` is inside one private range.
fn is_private_subnet(subnet: &Cidr) -> bool {
    match subnet.network() {
        IpAddr::V4(v4) => PRIVATE_V4.iter().any(|(network, prefix)| {
            subnet.prefix() >= *prefix && within_v4(v4, *network, *prefix)
        }),
        IpAddr::V6(v6) => {
            subnet.prefix() >= PRIVATE_V6.1 && within_v6(v6, PRIVATE_V6.0, PRIVATE_V6.1)
        }
    }
}

fn within_v4(ip: Ipv4Addr, network: [u8; 4], prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(network)) & mask
}

fn within_v6(ip: Ipv6Addr, network: u128, prefix: u8) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
    u128::from(ip) & mask == network & mask
}

fn classify(ip: IpAddr) -> Class {
    match ip {
        IpAddr::V4(v4) => {
            if let Some((_, _, kind)) = SPECIAL_V4
                .iter()
                .find(|(network, prefix, _)| within_v4(v4, *network, *prefix))
            {
                Class::Special(kind)
            } else if PRIVATE_V4
                .iter()
                .any(|(network, prefix)| within_v4(v4, *network, *prefix))
            {
                Class::Private
            } else {
                Class::Public
            }
        }
        IpAddr::V6(v6) => {
            if v6 == AWS_METADATA_V6 {
                Class::Special("cloud metadata address")
            } else if within_v6(v6, PRIVATE_V6.0, PRIVATE_V6.1) {
                Class::Private
            } else if !within_v6(v6, 0x2000 << 112, 3) {
                Class::Special("IPv6 address outside global unicast")
            } else if let Some((_, _, kind)) = SPECIAL_V6
                .iter()
                .find(|(network, prefix, _)| within_v6(v6, *network, *prefix))
            {
                Class::Special(kind)
            } else {
                Class::Public
            }
        }
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

    fn open_policy() -> EgressPolicy {
        EgressPolicy::new(
            vec![rule("example.com")],
            vec![cidr("10.20.0.0/16"), cidr("fd12:3456::/32")],
            vec![cidr("172.30.0.0/24"), cidr("172.31.0.2/32")],
        )
        .unwrap()
    }

    #[test]
    fn link_local_metadata_loopback_and_special_addresses_are_never_reached() {
        let policy = open_policy();
        for text in [
            "169.254.169.254",
            "169.254.0.1",
            "::ffff:169.254.169.254",
            "fd00:ec2::254",
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
            assert!(
                matches!(policy.unreachable(ip(text)), Some(Unreachable::Special(_))),
                "{text} is reachable"
            );
        }
    }

    #[test]
    fn private_ranges_are_denied_unless_allowed() {
        let policy = open_policy();
        for text in [
            "10.0.0.1",
            "10.21.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "fc00::1",
            "fd00::1",
            "fd12:3457::1",
            "::ffff:10.0.0.1",
        ] {
            assert_eq!(
                policy.unreachable(ip(text)),
                Some(Unreachable::Private),
                "{text}"
            );
        }
        for text in [
            "10.20.0.1",
            "10.20.255.255",
            "::ffff:10.20.3.4",
            "fd12:3456::9",
        ] {
            assert_eq!(policy.unreachable(ip(text)), None, "{text}");
        }
    }

    #[test]
    fn agentd_and_the_sandbox_network_are_never_reached() {
        let policy = open_policy();
        for text in [
            "172.30.0.2",
            "172.30.0.200",
            "::ffff:172.30.0.9",
            "172.31.0.2",
        ] {
            assert_eq!(
                policy.unreachable(ip(text)),
                Some(Unreachable::Own),
                "{text}"
            );
        }
        assert_eq!(
            policy.unreachable(ip("172.31.0.3")),
            Some(Unreachable::Private)
        );
    }

    #[test]
    fn public_addresses_are_reachable() {
        let policy = open_policy();
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
            assert_eq!(policy.unreachable(ip(text)), None, "{text}");
        }
        assert_eq!(Unreachable::Own.describe(), "agentd's own network");
        assert_eq!(Unreachable::Private.describe(), "private address");
    }

    #[test]
    fn allow_private_must_be_private_and_clear_of_agentd() {
        let own = vec![cidr("172.30.0.0/24"), cidr("172.31.0.2/32")];
        let check = |subnet: &str| {
            EgressPolicy::new(
                Vec::new(),
                vec![cidr("10.0.0.0/24"), cidr(subnet)],
                own.clone(),
            )
            .map(|_| ())
        };
        for (subnet, reason) in [
            ("0.0.0.0/0", "must be inside"),
            ("8.0.0.0/7", "must be inside"),
            ("169.254.0.0/16", "must be inside"),
            ("127.0.0.0/8", "must be inside"),
            ("100.64.0.0/10", "must be inside"),
            ("fe80::/10", "must be inside"),
            ("::/0", "must be inside"),
            ("fd00::/8", "fd00:ec2::254"),
            ("fd00:ec2::254/128", "fd00:ec2::254"),
            ("172.16.0.0/12", "must not overlap"),
            ("172.30.0.128/25", "must not overlap"),
            ("172.31.0.0/24", "must not overlap"),
        ] {
            let err = check(subnet).unwrap_err();
            assert_eq!(err.index, 1, "{subnet}");
            assert!(err.reason.contains(reason), "{subnet}: {err}");
        }
        for subnet in ["10.0.0.0/8", "192.168.5.0/24", "172.20.0.0/16", "fd12::/16"] {
            assert!(check(subnet).is_ok(), "{subnet}");
        }
        let policy = open_policy();
        assert_eq!(policy.rules(), [rule("example.com")]);
    }
}
