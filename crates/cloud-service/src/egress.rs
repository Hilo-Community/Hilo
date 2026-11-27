//! Outbound-host allowlist for the Cloud Service's own calls to Cloudflare (the only non-AWS
//! egress this service makes): the same shape as `agent-gateway`'s per-session proxy — an
//! exact-host allowlist validated at startup, DNS resolved then checked against
//! `agent_gateway::network::public_address` before connecting, and no silent fallback to an
//! unlisted host (`AGENTS.md` rule 1 extends to this server's own egress, not only to sandboxed
//! processes).

use std::net::SocketAddr;

use agent_gateway::network::{public_address, valid_host};

#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    #[error("{0} is not on the egress allowlist")]
    NotAllowed(String),
    #[error("{0} is not a routable DNS name")]
    InvalidHost(String),
    #[error("{0} resolved to a non-public address; refused")]
    NonPublicAddress(String),
    #[error("DNS resolution failed for {host}: {source}")]
    Resolve {
        host: String,
        #[source]
        source: std::io::Error,
    },
}

/// An exact-host allowlist for one outbound destination class (e.g. the Cloudflare API).
#[derive(Debug, Clone)]
pub struct EgressAllowlist {
    hosts: Vec<String>,
}

impl EgressAllowlist {
    /// Fails if any entry is not a plain DNS name: a silently-widened allowlist is worse than
    /// refusing to start.
    pub fn new(hosts: Vec<String>) -> Result<Self, EgressError> {
        for host in &hosts {
            if !valid_host(host) {
                return Err(EgressError::InvalidHost(host.clone()));
            }
        }
        Ok(Self { hosts })
    }

    pub fn allows(&self, host: &str) -> bool {
        self.hosts.iter().any(|allowed| allowed == host)
    }

    /// Checks the allowlist, resolves `host`, and checks every resolved address is public
    /// (refuses loopback, RFC1918, link-local, cloud-metadata, etc.) before the caller connects.
    pub async fn verify(&self, host: &str) -> Result<Vec<SocketAddr>, EgressError> {
        if !self.allows(host) {
            return Err(EgressError::NotAllowed(host.to_string()));
        }
        if !valid_host(host) {
            return Err(EgressError::InvalidHost(host.to_string()));
        }
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, 443))
            .await
            .map_err(|source| EgressError::Resolve {
                host: host.to_string(),
                source,
            })?
            .collect();
        if addresses.is_empty() || !addresses.iter().all(|addr| public_address(addr.ip())) {
            return Err(EgressError::NonPublicAddress(host.to_string()));
        }
        Ok(addresses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_is_refused_for_an_ip_literal_or_local_suffix() {
        assert!(EgressAllowlist::new(vec!["127.0.0.1".into()]).is_err());
        assert!(EgressAllowlist::new(vec!["api.internal".into()]).is_err());
    }

    #[test]
    fn an_unlisted_host_is_not_allowed() {
        let allowlist = EgressAllowlist::new(vec!["api.cloudflare.com".into()]).unwrap();
        assert!(allowlist.allows("api.cloudflare.com"));
        assert!(!allowlist.allows("evil.example.com"));
    }

    #[tokio::test]
    async fn verify_refuses_a_host_outside_the_allowlist_without_resolving_it() {
        let allowlist = EgressAllowlist::new(vec!["api.cloudflare.com".into()]).unwrap();
        let result = allowlist.verify("attacker.example.com").await;
        assert!(matches!(result, Err(EgressError::NotAllowed(_))));
    }
}
