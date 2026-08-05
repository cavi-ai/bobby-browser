use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::net::lookup_host;
use types::CommandError;
use url::Url;

use crate::eligibility::{policy_error, validate_http_url};

#[derive(Debug, Clone)]
pub struct NetworkPolicy {
    pub allow_loopback: bool,
    pub allow_private_network: bool,
    pub max_redirects: usize,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub max_download_bytes: usize,
    pub request_timeout_ms: u64,
    pub max_concurrent_requests: usize,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            allow_loopback: false,
            allow_private_network: false,
            max_redirects: 5,
            max_header_bytes: 64 * 1024,
            max_body_bytes: 8 * 1024 * 1024,
            max_download_bytes: 64 * 1024 * 1024,
            request_timeout_ms: 30_000,
            max_concurrent_requests: 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDestination {
    url: Url,
    addresses: Vec<SocketAddr>,
}

impl ValidatedDestination {
    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }
}

#[derive(Debug, Clone)]
pub struct DestinationPolicy {
    network: NetworkPolicy,
}

impl DestinationPolicy {
    pub fn new(network: NetworkPolicy) -> Self {
        Self { network }
    }

    pub async fn resolve_and_validate(
        &self,
        input: &str,
    ) -> Result<ValidatedDestination, CommandError> {
        validate_http_url(input)?;
        let url = Url::parse(input).map_err(|_| policy_error("URL is invalid"))?;
        let host = url
            .host_str()
            .ok_or_else(|| policy_error("URL must include a host"))?;
        let lookup_host_name = host.trim_start_matches('[').trim_end_matches(']');
        let port = url
            .port_or_known_default()
            .ok_or_else(|| policy_error("URL does not have a valid port"))?;
        let addresses: Vec<_> = lookup_host((lookup_host_name, port))
            .await
            .map_err(|_| policy_error("destination resolution failed"))?
            .collect();

        self.validate_resolved(url, addresses)
    }

    pub fn validate_resolved(
        &self,
        url: Url,
        addresses: Vec<SocketAddr>,
    ) -> Result<ValidatedDestination, CommandError> {
        validate_http_url(url.as_str())?;
        if addresses.is_empty() {
            return Err(policy_error("destination resolved to no addresses"));
        }
        if addresses
            .iter()
            .any(|address| !self.ip_is_allowed(address.ip()))
        {
            return Err(policy_error("destination address is not permitted"));
        }

        Ok(ValidatedDestination { url, addresses })
    }

    fn ip_is_allowed(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ip) => self.ipv4_is_allowed(ip),
            IpAddr::V6(ip) => {
                if let Some(ipv4) = ip.to_ipv4_mapped().or_else(|| ipv4_compatible(&ip)) {
                    return self.ipv4_is_allowed(ipv4);
                }
                // Transitional tunnels embed an IPv4 address the v4 policy
                // never sees: 6to4 (2002::/16) carries it in bits 16..48,
                // Teredo (2001::/32) in the tail. Reject both outright.
                let segments = ip.segments();
                if segments[0] == 0x2002 || (segments[0] == 0x2001 && segments[1] == 0) {
                    return false;
                }
                if ip.is_loopback() {
                    return self.network.allow_loopback;
                }
                if ip.is_unspecified() || ip.is_multicast() || ip.is_unicast_link_local() {
                    return false;
                }
                !ip.is_unique_local() || self.network.allow_private_network
            }
        }
    }

    fn ipv4_is_allowed(&self, ip: Ipv4Addr) -> bool {
        if ip.is_loopback() {
            return self.network.allow_loopback;
        }
        if ip.is_unspecified()
            || ip.is_multicast()
            || ip.is_link_local()
            || ip.is_broadcast()
            || is_cgnat(ip)
        {
            return false;
        }
        !ip.is_private() || self.network.allow_private_network
    }
}

/// IPv4-compatible (`::a.b.c.d`): deprecated, but still routed by some
/// stacks, so it must hit the v4 policy rather than slip through as "v6".
fn ipv4_compatible(ip: &std::net::Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();
    if segments[..6] == [0; 6] && segments[6] != 0 {
        let [a, b, c, d] = [
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        ];
        Some(Ipv4Addr::new(a, b, c, d))
    } else {
        None
    }
}

/// CGNAT/shared address space (100.64.0.0/10): not public, not RFC1918
/// private, and never a legitimate automation target.
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}
