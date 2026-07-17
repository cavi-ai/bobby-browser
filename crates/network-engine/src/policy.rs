use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::net::lookup_host;
use types::CommandError;
use url::Url;

use crate::eligibility::policy_error;

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
    pub url: Url,
    pub addresses: Vec<SocketAddr>,
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
        let url = Url::parse(input).map_err(|_| policy_error("URL is invalid"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(policy_error("URL scheme is not permitted"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(policy_error("credentials in URLs are not permitted"));
        }
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
                if let Some(ipv4) = ip.to_ipv4_mapped() {
                    return self.ipv4_is_allowed(ipv4);
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
        if ip.is_unspecified() || ip.is_multicast() || ip.is_link_local() {
            return false;
        }
        !ip.is_private() || self.network.allow_private_network
    }
}
