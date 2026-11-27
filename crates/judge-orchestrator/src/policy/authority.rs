//! Only the Host owns signing material; wire envelopes carry data and MACs, never keys.

use host_core::{Approval, Evidence, TaskSpec};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const TASK_DOMAIN: &[u8] = b"hilo.host.task.v1\0";
const APPROVAL_DOMAIN: &[u8] = b"hilo.host.approval.v1\0";
const EVIDENCE_DOMAIN: &[u8] = b"hilo.host.evidence.v1\0";

#[derive(Debug, Error)]
pub enum AuthorityError {
    #[error("Host entropy unavailable: {0}")]
    Entropy(String),
    #[error("cannot encode signed contract: {0}")]
    Encoding(#[from] bincode::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedTaskSpec {
    pub spec: TaskSpec,
    pub mac: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedApproval {
    pub approval: Approval,
    pub mac: [u8; 32],
}

/// Evidence the Judge produced: only the Host that ran the checks can have signed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEvidence {
    pub evidence: Evidence,
    pub mac: [u8; 32],
}

/// Keep this in the trusted Host, never in a model, extension, or MCP-facing interface.
pub struct HostAuthority {
    key: [u8; 32],
}

impl std::fmt::Debug for HostAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostAuthority").finish_non_exhaustive()
    }
}

impl HostAuthority {
    pub fn new() -> Result<Self, AuthorityError> {
        let mut key = [0; 32];
        getrandom::fill(&mut key).map_err(|error| AuthorityError::Entropy(error.to_string()))?;
        Ok(Self { key })
    }

    fn mac(&self, domain: &[u8], value: &impl Serialize) -> Result<[u8; 32], AuthorityError> {
        let mut bytes = domain.to_vec();
        bytes.extend(bincode::serialize(value)?);
        Ok(*blake3::keyed_hash(&self.key, &bytes).as_bytes())
    }

    pub fn sign_task(&self, spec: TaskSpec) -> Result<SignedTaskSpec, AuthorityError> {
        let mac = self.mac(TASK_DOMAIN, &spec)?;
        Ok(SignedTaskSpec { spec, mac })
    }

    pub fn sign_approval(&self, approval: Approval) -> Result<SignedApproval, AuthorityError> {
        let mac = self.mac(APPROVAL_DOMAIN, &approval)?;
        Ok(SignedApproval { approval, mac })
    }

    pub fn verify_task(&self, signed: &SignedTaskSpec) -> bool {
        self.mac(TASK_DOMAIN, &signed.spec)
            .is_ok_and(|expected| constant_time_eq::constant_time_eq_32(&expected, &signed.mac))
    }

    pub fn verify_approval(&self, signed: &SignedApproval) -> bool {
        self.mac(APPROVAL_DOMAIN, &signed.approval)
            .is_ok_and(|expected| constant_time_eq::constant_time_eq_32(&expected, &signed.mac))
    }

    pub fn sign_evidence(&self, evidence: Evidence) -> Result<SignedEvidence, AuthorityError> {
        let mac = self.mac(EVIDENCE_DOMAIN, &evidence)?;
        Ok(SignedEvidence { evidence, mac })
    }

    pub fn verify_evidence(&self, signed: &SignedEvidence) -> bool {
        self.mac(EVIDENCE_DOMAIN, &signed.evidence)
            .is_ok_and(|expected| constant_time_eq::constant_time_eq_32(&expected, &signed.mac))
    }
}
