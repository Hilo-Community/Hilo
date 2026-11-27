//! The deterministic Judge (`LOT-015`): agent changes are compiled against the exact current
//! text, verified in the native OS sandbox, and given a Hilo state bound to the content hash
//! they were verified on.

mod blocks;
mod compiler;
mod correction;
mod diagnostics;
mod impact;
mod pipeline;
mod proposal;
mod reverify;
mod review;
mod tools;

pub use blocks::{block_end, enclosing_symbol, resolve_symbol, BlockLookup, SymbolBlock};
pub use compiler::{
    parse, result_digest, Anchoring, AstChangeSetCompiler, CompileError, CompiledChangeSet,
    CompiledEdit, CompiledFile,
};
pub use correction::{
    agent_feedback, CorrectionDecision, CorrectionLoop, StopReason, MAX_AUTO_RETRIES,
};
pub use diagnostics::{parse_biome, parse_tsc, parse_vitest};
pub use impact::{impacted_tests, touched_symbols, IMPACT_DEPTH};
pub use pipeline::{
    JudgeError, JudgeOutcome, JudgePipeline, JudgeProgress, PreviewSnapshot, StepKind, StepReport,
    ToolCommand, Verification, VerificationPlan,
};
pub use proposal::{changeset_from_proposals, FileProposal};
pub use reverify::{reverified_ranges, StaleReverifier, REVERIFY_IDLE};
pub use review::{
    adjustment_prompt, apply_sequential, matches_plan, plan_accept, sequential_changes, AcceptPlan,
    AcceptRefusal, JudgedChange, ReviewFile,
};
pub use tools::{node_verification_plan, NodeToolchain};
