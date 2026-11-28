//! Which peers may name a client in `X-Forwarded-For`.
//!
//! A Space behind a TLS proxy receives every request from the proxy, so the
//! TCP peer is not the client. The operator names the proxies the Space
//! believes; a request from a named proxy carries the client address in the
//! last forwarded value, and a request from any other peer is counted by its
//! own address. Reading only named proxies keeps a caller from choosing its
//! own address for a rate limit.
//!
//! The chain is single-hop by contract: the public proxy overwrites or
//! normalizes `X-Forwarded-For`, so the last value it sends is the client.
//! A proxy that forwards an earlier chain unchanged names itself instead, and
//! the Space falls back to the peer.

use std::io;
use std::net::{IpAddr, ToSocketAddrs};

use axum::http::HeaderMap;

/// The most proxies one Space may name.
const MAX_PROXIES: usize = 64;

/// One address range a Space believes as a proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Trusted {
    /// The first address of the range.
    network: IpAddr,
    /// The prefix length of the range.
    prefix: u8,
}

/// The peers whose forwarded client address a Space reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrustedProxies {
    entries: Vec<Trusted>,
}

impl TrustedProxies {
    /// Reads the proxies from `NEHA_TRUSTED_PROXIES`.
    ///
    /// The value is a comma-separated list of addresses, CIDR ranges, or
    /// hostnames. A hostname is resolved once, at start. An unset or empty
    /// value trusts no proxy; anything else refuses the start when it cannot
    /// be used.
    pub(crate) fn from_env() -> Result<Option<Self>, String> {
        let Some(value) = std::env::var_os("NEHA_TRUSTED_PROXIES") else {
            return Ok(None);
        };
        let value = value
            .to_str()
            .ok_or_else(|| "NEHA_TRUSTED_PROXIES is not UTF-8".to_owned())?;

        Self::parse(value).map(|proxies| proxies.filter(|proxies| !proxies.entries.is_empty()))
    }

    /// Reads a list of proxies, resolving hostnames through this machine.
    fn parse(value: &str) -> Result<Option<Self>, String> {
        Self::parse_with(value, |host| {
            (host, 0).to_socket_addrs().map(|addresses| {
                addresses
                    .map(|address| address.ip())
                    .collect::<Vec<IpAddr>>()
            })
        })
    }

    /// Reads a list of proxies, resolving hostnames through `resolve`.
    fn parse_with(
        value: &str,
        mut resolve: impl FnMut(&str) -> io::Result<Vec<IpAddr>>,
    ) -> Result<Option<Self>, String> {
        let mut entries = Vec::new();
        for entry in value.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if entries.len() >= MAX_PROXIES {
                return Err(format!(
                    "NEHA_TRUSTED_PROXIES names more than {MAX_PROXIES} proxies"
                ));
            }
            match parse_entry(entry, &mut resolve) {
                Ok(parsed) => entries.extend(parsed),
                Err(reason) => {
                    return Err(format!(
                        "NEHA_TRUSTED_PROXIES has an entry that cannot be used: {reason}"
                    ));
                }
            }
            if entries.len() > MAX_PROXIES {
                return Err(format!(
                    "NEHA_TRUSTED_PROXIES names more than {MAX_PROXIES} addresses"
                ));
            }
        }

        Ok(Some(Self { entries }))
    }

    /// Returns whether `address` is one of the named proxies.
    fn allows(&self, address: IpAddr) -> bool {
        self.entries.iter().any(|entry| entry.contains(address))
    }
}

impl Trusted {
    /// Returns whether `address` is inside this range.
    fn contains(&self, address: IpAddr) -> bool {
        match (self.network, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                matches_prefix(&network.octets(), &address.octets(), self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                matches_prefix(&network.octets(), &address.octets(), self.prefix)
            }
            (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// Parses one entry: an address, a CIDR range, or a hostname.
fn parse_entry(
    entry: &str,
    resolve: &mut impl FnMut(&str) -> io::Result<Vec<IpAddr>>,
) -> Result<Vec<Trusted>, String> {
    if let Some((address, prefix)) = entry.split_once('/') {
        let address: IpAddr = address
            .trim()
            .parse()
            .map_err(|_| format!("{entry} is not an address with a prefix"))?;
        let prefix: u8 = prefix
            .trim()
            .parse()
            .map_err(|_| format!("{entry} has no prefix length"))?;
        let maximum = match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix > maximum {
            return Err(format!("{entry} has a prefix longer than {maximum}"));
        }
        return Ok(vec![Trusted {
            network: address,
            prefix,
        }]);
    }

    if let Ok(address) = entry.parse::<IpAddr>() {
        let prefix = match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        return Ok(vec![Trusted {
            network: address,
            prefix,
        }]);
    }

    if entry.contains(char::is_whitespace) || entry.contains('[') || entry.contains(']') {
        return Err(format!("{entry} is not an address, range, or hostname"));
    }
    let addresses =
        resolve(entry).map_err(|error| format!("cannot resolve proxy {entry}: {error}"))?;
    if addresses.is_empty() {
        return Err(format!("proxy {entry} resolves to no address"));
    }

    Ok(addresses
        .into_iter()
        .map(|address| {
            let prefix = match address {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            Trusted {
                network: address,
                prefix,
            }
        })
        .collect())
}

/// Returns whether the first `prefix` bits of two addresses are equal.
fn matches_prefix(network: &[u8], address: &[u8], prefix: u8) -> bool {
    let whole = usize::from(prefix / 8);
    if network[..whole] != address[..whole] {
        return false;
    }
    let rest = prefix % 8;
    rest == 0 || (network[whole] ^ address[whole]) & (0xFF << (8 - rest)) == 0
}

/// Returns the client address one request came from.
///
/// A loopback peer is trusted the way it always was; any other peer must be
/// named. From a trusted peer the Space reads the last forwarded value, so a
/// proxy that overwrites or normalizes `X-Forwarded-For` names the client. A
/// value that is absent, malformed, loopback, or itself a proxy falls back to
/// the peer, so a caller never chooses its own address.
pub(crate) fn client_ip(
    peer: IpAddr,
    headers: &HeaderMap,
    trusted: Option<&TrustedProxies>,
) -> IpAddr {
    if !peer.is_loopback() && !trusted.is_some_and(|trusted| trusted.allows(peer)) {
        return peer;
    }
    // Select the final raw field first. A malformed field must fall back to the
    // peer, not expose an earlier client-supplied field.
    let Some(value) = headers.get_all("x-forwarded-for").iter().next_back() else {
        return peer;
    };
    let Some(value) = value.to_str().ok() else {
        return peer;
    };

    forwarded_address(value)
        .filter(|address| !address.is_loopback())
        .filter(|address| !trusted.is_some_and(|trusted| trusted.allows(*address)))
        .unwrap_or(peer)
}

/// Returns the last address in one forwarded field, or `None` when it is not
/// an address. Earlier values are the client's claim, not the proxy's.
fn forwarded_address(value: &str) -> Option<IpAddr> {
    let last = value.split(',').next_back()?.trim().trim_matches('"');
    let last = last
        .strip_prefix('[')
        .and_then(|last| last.strip_suffix(']'))
        .unwrap_or(last);
    last.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use axum::http::{HeaderMap, HeaderValue};

    use super::{TrustedProxies, client_ip, forwarded_address};

    fn ip(value: [u8; 4]) -> IpAddr {
        IpAddr::from(Ipv4Addr::from(value))
    }

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_str(value).unwrap());
        headers
    }

    fn trusted(value: &str) -> TrustedProxies {
        TrustedProxies::parse_with(value, |_| Ok(vec![ip([192, 0, 2, 9])]))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_list_parses_addresses_ranges_and_hostnames() {
        let proxies = TrustedProxies::parse_with("127.0.0.1, 10.0.0.0/8, proxy.internal", |host| {
            assert_eq!(host, "proxy.internal");
            Ok(vec![ip([192, 0, 2, 9]), ip([198, 51, 100, 4])])
        })
        .unwrap()
        .unwrap();
        assert!(proxies.allows(ip([127, 0, 0, 1])));
        assert!(proxies.allows(ip([10, 255, 0, 2])));
        assert!(!proxies.allows(ip([11, 0, 0, 2])));
        assert!(proxies.allows(ip([192, 0, 2, 9])));
        assert!(proxies.allows(ip([198, 51, 100, 4])));

        let prefix_of_eight = TrustedProxies::parse_with("10.0.0.0/8", |_| unreachable!())
            .unwrap()
            .unwrap();
        assert!(prefix_of_eight.allows(ip([10, 1, 2, 3])));
    }

    #[test]
    fn a_prefix_match_uses_only_the_named_bits() {
        let proxies = TrustedProxies::parse_with("192.0.2.0/25", |_| unreachable!())
            .unwrap()
            .unwrap();
        assert!(proxies.allows(ip([192, 0, 2, 0])));
        assert!(proxies.allows(ip([192, 0, 2, 127])));
        assert!(!proxies.allows(ip([192, 0, 2, 128])));
        assert!(!proxies.allows(ip([192, 0, 2, 255])));
    }

    #[test]
    fn an_empty_or_unusable_list_is_refused() {
        let empty = TrustedProxies::parse_with(" , ", |_| unreachable!()).unwrap();
        assert!(empty.is_some_and(|proxies| proxies.entries.is_empty()));

        assert!(
            TrustedProxies::parse_with("10.0.0.0/33", |_| unreachable!())
                .unwrap_err()
                .contains("prefix")
        );
        assert!(
            TrustedProxies::parse_with("not a host", |_| unreachable!())
                .unwrap_err()
                .contains("not an address")
        );
        assert!(
            TrustedProxies::parse_with("missing.internal", |_| Err(std::io::Error::from(
                std::io::ErrorKind::NotFound
            )))
            .unwrap_err()
            .contains("cannot resolve")
        );

        let many = (0..=64)
            .map(|last| ip([10, 0, 0, last]))
            .collect::<Vec<_>>();
        assert!(
            TrustedProxies::parse_with("many.internal", |_| Ok(many.clone()))
                .unwrap_err()
                .contains("more than")
        );
    }

    #[test]
    fn a_forwarded_address_is_read_only_from_a_trusted_peer() {
        let proxy = ip([127, 0, 0, 1]);
        let remote = ip([10, 9, 9, 9]);
        let client = ip([203, 0, 113, 7]);

        assert_eq!(
            client_ip(proxy, &headers("10.0.0.4, 203.0.113.7"), None),
            client
        );
        assert_eq!(client_ip(remote, &headers("203.0.113.7"), None), remote);
        assert_eq!(client_ip(proxy, &headers("garbage"), None), proxy);
        assert_eq!(
            client_ip(proxy, &headers("203.0.113.7, 127.0.0.1"), None),
            proxy
        );

        let proxies = trusted("10.0.0.1");
        assert_eq!(
            client_ip(ip([10, 0, 0, 1]), &headers("198.51.100.9"), Some(&proxies)),
            ip([198, 51, 100, 9])
        );
        assert_eq!(
            client_ip(ip([10, 0, 0, 2]), &headers("198.51.100.9"), Some(&proxies)),
            ip([10, 0, 0, 2])
        );
    }

    #[test]
    fn only_the_last_hop_names_the_client() {
        let proxies = trusted("10.0.0.1");
        // A proxy that overwrites the header, as the deployment guide asks.
        assert_eq!(
            client_ip(
                ip([10, 0, 0, 1]),
                &headers("1.1.1.1, 203.0.113.7"),
                Some(&proxies)
            ),
            ip([203, 0, 113, 7])
        );
        // A proxy that forwards a chain unchanged names itself last; the Space
        // must not treat an earlier, client-supplied value as the client.
        assert_eq!(
            client_ip(
                ip([10, 0, 0, 1]),
                &headers("203.0.113.7, 10.0.0.1"),
                Some(&proxies)
            ),
            ip([10, 0, 0, 1])
        );
        assert_eq!(
            client_ip(
                ip([10, 0, 0, 1]),
                &headers("203.0.113.7, garbage"),
                Some(&proxies)
            ),
            ip([10, 0, 0, 1])
        );
    }

    #[test]
    fn a_forwarded_address_survives_several_header_fields() {
        let mut fields = HeaderMap::new();
        fields.append("x-forwarded-for", HeaderValue::from_static("198.51.100.9"));
        fields.append("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        assert_eq!(
            client_ip(ip([127, 0, 0, 1]), &fields, None),
            ip([203, 0, 113, 7])
        );

        let mut invalid = HeaderMap::new();
        invalid.append("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        invalid.append(
            "x-forwarded-for",
            HeaderValue::from_bytes(&[0xFF]).expect("obs-text is a header value"),
        );
        assert_eq!(
            client_ip(ip([127, 0, 0, 1]), &invalid, None),
            ip([127, 0, 0, 1])
        );
    }

    #[test]
    fn a_forwarded_address_drops_readability_wrappers() {
        assert_eq!(
            forwarded_address(" 203.0.113.7 "),
            Some(ip([203, 0, 113, 7]))
        );
        assert_eq!(
            forwarded_address("\"203.0.113.7\""),
            Some(ip([203, 0, 113, 7]))
        );
        assert_eq!(
            forwarded_address("[2001:db8::1]"),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(forwarded_address("garbage"), None);
    }
}
