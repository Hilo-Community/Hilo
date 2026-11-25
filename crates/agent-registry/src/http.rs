//! Fetching over HTTPS. Everything downloaded is bounded by the caller and, for code, checked
//! against a digest published in the catalog.

use std::io::Read;
use std::time::Duration;

use crate::error::RegistryError;

/// Whether a download may be redirected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    /// The catalog itself: nothing vouches for it, so it is read from the one address and no
    /// other.
    Refuse,
    /// An archive: releases are served from other hosts, and its SHA-256 vouches for it.
    Follow,
}

pub trait Fetch: Send + Sync {
    fn open(&self, url: &str, redirects: Redirects) -> Result<Box<dyn Read + Send>, RegistryError>;
}

pub struct HttpFetch {
    strict: ureq::Agent,
    following: ureq::Agent,
}

impl Default for HttpFetch {
    fn default() -> Self {
        let build = |redirects: u32| {
            ureq::AgentBuilder::new()
                .user_agent("Hilo")
                .redirects(redirects)
                .timeout_connect(Duration::from_secs(15))
                // Between two reads, not for the whole download.
                .timeout_read(Duration::from_secs(60))
                .timeout_write(Duration::from_secs(30))
                .build()
        };
        Self {
            strict: build(0),
            following: build(5),
        }
    }
}

impl Fetch for HttpFetch {
    fn open(&self, url: &str, redirects: Redirects) -> Result<Box<dyn Read + Send>, RegistryError> {
        if !url.starts_with("https://") {
            return Err(RegistryError::Download(format!(
                "{url} is not an HTTPS address"
            )));
        }
        let agent = match redirects {
            Redirects::Refuse => &self.strict,
            Redirects::Follow => &self.following,
        };
        match agent.get(url).call() {
            Ok(response) => Ok(Box::new(response.into_reader())),
            Err(ureq::Error::Status(code, _)) => {
                Err(RegistryError::Download(format!("{url} answered {code}")))
            }
            Err(error) => Err(RegistryError::Download(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_refused_before_any_connection() {
        for url in [
            "http://example.org/a.zip",
            "ftp://example.org/a",
            "file:///etc/passwd",
            "example.org",
        ] {
            let error = HttpFetch::default()
                .open(url, Redirects::Follow)
                .err()
                .expect(url);
            assert!(error.to_string().contains("HTTPS"), "{url}: {error}");
        }
    }

    /// Reads the real catalog. Run by hand: `cargo test -p agent-registry -- --ignored`.
    #[test]
    #[ignore = "needs the network"]
    fn the_real_catalog_parses() {
        let mut body = String::new();
        HttpFetch::default()
            .open(crate::catalog::REGISTRY_URL, Redirects::Refuse)
            .unwrap()
            .take(4 * 1024 * 1024)
            .read_to_string(&mut body)
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
        let entries =
            crate::catalog::parse_catalog(&payload, crate::catalog::platform_target().as_deref())
                .unwrap();
        assert!(entries.len() > 10, "{} entries", entries.len());
        assert!(entries.iter().any(|entry| entry.id == "codex-acp"));
    }
}
