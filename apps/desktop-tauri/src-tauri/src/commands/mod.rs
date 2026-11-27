//! Tauri commands, grouped by concern. Each one is registered in `crate::run`.

pub mod agent;
pub mod agent_account;
pub mod agent_registry;
pub mod agent_transcripts;
pub mod completion;
pub mod context_index;
pub mod debug;
pub mod documents;
pub mod judge;
pub mod language;
pub mod migration;
pub mod publish;
pub mod search;
pub mod semantic;
pub mod session_cache;
pub mod terminal;
pub mod trust;
pub mod window;
pub mod workspace;
