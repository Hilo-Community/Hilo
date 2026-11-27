//! Pre-flight memory check: which local model this machine can load without swapping.
use crate::registry::{ModelEntry, ModelRegistry};

/// Memory the system can give a new process now, in bytes. On macOS `sysinfo` subtracts the
/// compressor's pages from the free ones and can report 0 on a busy machine that would still
/// load a model; the memory not in use is the better estimate there.
pub fn available_memory() -> u64 {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    system
        .available_memory()
        .max(system.total_memory().saturating_sub(system.used_memory()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recommendation {
    /// The largest local model that fits.
    Local { model_id: String },
    /// No local model fits: use a key of your own or the managed cloud instead.
    NotEnoughMemory { needed: u64, available: u64 },
}

/// Picks the most capable local model whose memory need fits in `available`.
pub fn recommend(registry: &ModelRegistry, available: u64) -> Recommendation {
    let mut local: Vec<&ModelEntry> = registry
        .entries()
        .iter()
        .filter(|entry| entry.destination == host_core::AuthorizedDestination::LocalOnly)
        .collect();
    local.sort_by_key(|entry| entry.min_free_memory_bytes);
    match local
        .iter()
        .rev()
        .find(|entry| entry.min_free_memory_bytes <= available)
    {
        Some(entry) => Recommendation::Local {
            model_id: entry.id.clone(),
        },
        None => Recommendation::NotEnoughMemory {
            needed: local.first().map_or(0, |entry| entry.min_free_memory_bytes),
            available,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn the_largest_model_that_fits_is_recommended() {
        let registry = ModelRegistry::builtin();
        assert_eq!(
            recommend(&registry, 16 * GIB),
            Recommendation::Local {
                model_id: "ollama/qwen2.5-coder:7b-base".into()
            }
        );
        assert_eq!(
            recommend(&registry, 3 * GIB),
            Recommendation::Local {
                model_id: "ollama/qwen2.5-coder:1.5b-base".into()
            }
        );
        assert_eq!(
            recommend(&registry, GIB),
            Recommendation::NotEnoughMemory {
                needed: 2 * GIB,
                available: GIB
            }
        );
    }

    #[test]
    fn this_machine_reports_some_memory() {
        assert!(available_memory() > 0);
    }
}
