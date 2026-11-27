//! The automatic correction loop (`LOT-015`): a `🔴` verdict goes back to the agent with its
//! structured evidence, at most twice, and stops at once when the same failure comes back.

use host_core::HiloValidationState;

use super::pipeline::{JudgeOutcome, StepKind};

/// Automatic correction cycles after the first attempt.
pub const MAX_AUTO_RETRIES: u32 = 2;
/// Longest feedback sent to the agent.
const MAX_FEEDBACK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The last allowed retry failed too.
    RetriesExhausted,
    /// The attempt failed exactly as the previous one: retrying would not converge.
    SameFailureRepeated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrectionDecision {
    /// Not `🔴`: the change waits for the human review (`🟡`, with its evidence).
    AwaitReview,
    /// Send `feedback` to the agent; its next proposal is attempt `next_attempt`.
    Retry { next_attempt: u32, feedback: String },
    /// Final `🔴`: no further automatic attempt.
    Stop(StopReason),
}

#[derive(Debug, Clone, Default)]
pub struct CorrectionLoop {
    retries: u32,
    last_failure: Option<String>,
}

impl CorrectionLoop {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The attempt the next verification is (1 for the first).
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.retries + 1
    }

    #[must_use]
    pub const fn retries_used(&self) -> u32 {
        self.retries
    }

    pub fn record(&mut self, outcome: &JudgeOutcome) -> CorrectionDecision {
        if outcome.verdict() != HiloValidationState::FailedRed {
            return CorrectionDecision::AwaitReview;
        }
        let repeated =
            outcome.failure_signature.is_some() && outcome.failure_signature == self.last_failure;
        self.last_failure.clone_from(&outcome.failure_signature);
        if repeated {
            return CorrectionDecision::Stop(StopReason::SameFailureRepeated);
        }
        if self.retries >= MAX_AUTO_RETRIES {
            return CorrectionDecision::Stop(StopReason::RetriesExhausted);
        }
        let failed_attempt = self.attempt();
        self.retries += 1;
        CorrectionDecision::Retry {
            next_attempt: self.attempt(),
            feedback: agent_feedback(outcome, failed_attempt, MAX_AUTO_RETRIES - self.retries),
        }
    }
}

/// The evidence of a failed attempt, as the agent reads it. Tool output is quoted as data.
#[must_use]
pub fn agent_feedback(outcome: &JudgeOutcome, attempt: u32, retries_left: u32) -> String {
    let mut text = format!(
        "Hilo Judge: attempt {attempt} failed (✕ Failed). Nothing was applied to the user's files. \
         Fix the problems below and propose the corrected files again ({retries_left} automatic \
         retr{} left).\n",
        if retries_left == 1 { "y" } else { "ies" }
    );
    for step in &outcome.steps {
        let name = match step.kind {
            StepKind::ImpactedTests => "Impacted tests",
            StepKind::Lint => "Biome",
            StepKind::TypeCheck => "TypeScript",
            StepKind::Preview => "Component preview (runtime)",
            StepKind::FullSuite => "Full test suite",
        };
        if step.passed {
            text.push_str(&format!("- {name}: passed\n"));
            continue;
        }
        text.push_str(&format!(
            "- {name}: FAILED (exit code {})\n",
            step.record.exit_code
        ));
        for item in &step.items {
            let place = if item.file_path.is_empty() {
                String::new()
            } else {
                format!(
                    "{}:{}:{}: ",
                    item.file_path,
                    item.line_col.line + 1,
                    item.line_col.col_utf16 + 1
                )
            };
            for (index, line) in item.message.lines().enumerate() {
                let prefix = if index == 0 {
                    format!("  {place}[{}] ", item.code)
                } else {
                    "    ".to_string()
                };
                text.push_str(&prefix);
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    if text.len() > MAX_FEEDBACK_BYTES {
        let mut cut = MAX_FEEDBACK_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n…\n");
    }
    text
}
