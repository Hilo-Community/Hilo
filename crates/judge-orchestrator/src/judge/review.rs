//! Human review of a judged change (`LOT-015`): `[✓ Accept]`, `[✕ Reject]`,
//! `[Request Adjustment]`.
//!
//! Accepting compiles the change again against the text **as it is now**: the user may have
//! typed since the Judge ran. A target the user edited is a conflict, shown for review and
//! never applied. Otherwise the plan carries the exact edits and the state of each changed
//! range: `🟢` only when the resulting content is byte for byte the one the evidence was
//! signed for; a change that had to be rebased over the user's edits is `🟠` until the Judge
//! has verified the new content.

use host_core::{
    compute_rope_blake3_hex, ropey::Rope, utf16_to_char_idx, utf8_span_to_utf16, ChangeSet,
    HiloValidationState, OffsetConversionError, TextChange, Utf16Span,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::compiler::{parse, AstChangeSetCompiler, CompileError, CompiledFile};
use crate::policy::{HostAuthority, SignedEvidence};

/// A judged change awaiting the human decision.
#[derive(Debug, Clone)]
pub struct JudgedChange {
    pub review_id: String,
    pub changeset: ChangeSet,
    pub files: Vec<ReviewFile>,
    pub evidence: SignedEvidence,
    /// Verifications run for this change (1 + automatic retries).
    pub attempts: u32,
}

/// One file of a judged change: the text it was judged against and the text that was tested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    pub path: String,
    pub base_text: String,
    pub proposed_text: String,
}

impl ReviewFile {
    #[must_use]
    pub fn from_compiled(base_text: &str, compiled: &CompiledFile) -> Self {
        Self {
            path: compiled.file_path.clone(),
            base_text: base_text.to_string(),
            proposed_text: compiled.result_text.clone(),
        }
    }
}

/// The edits to apply to one document, in the coordinates of its current text (ascending and
/// disjoint, as CodeMirror dispatches them), and what the document must look like after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptPlan {
    pub file_path: String,
    pub base_content_hash: String,
    pub changes: Vec<TextChange>,
    pub result_content_hash: String,
    /// The changed ranges in the resulting text and their state.
    pub ranges: Vec<(Utf16Span, HiloValidationState)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum AcceptRefusal {
    #[error("the evidence was not signed by this Host")]
    ForgedEvidence,
    #[error("{0} is not part of this change")]
    UnknownFile(String),
    /// The user edited what the change replaces.
    #[error("conflict: {reason}")]
    Conflict {
        path: String,
        reason: String,
        current_text: String,
        proposed_text: String,
    },
    #[error("{0}")]
    Invalid(String),
}

/// Plans accepting `change` for `path`, whose text is now `current`.
pub fn plan_accept(
    authority: &HostAuthority,
    change: &JudgedChange,
    path: &str,
    current: &Rope,
) -> Result<AcceptPlan, AcceptRefusal> {
    if !authority.verify_evidence(&change.evidence) {
        return Err(AcceptRefusal::ForgedEvidence);
    }
    let file = change
        .files
        .iter()
        .find(|file| file.path == path)
        .ok_or_else(|| AcceptRefusal::UnknownFile(path.to_string()))?;
    let compiled =
        AstChangeSetCompiler::compile(current, &parse(current, path), &change.changeset, path)
            .map_err(|error| refusal(error, path, current, file))?;
    let evidence = &change.evidence.evidence;
    let state = if evidence.satisfies_validated_green_for_file(path, &compiled.result_content_hash)
    {
        HiloValidationState::ValidatedGreen
    } else {
        match evidence.verdict {
            HiloValidationState::FailedRed => HiloValidationState::FailedRed,
            // Verified, but not this content: the Judge must check the rebased text first.
            HiloValidationState::ValidatedGreen => HiloValidationState::StaleOrange,
            _ => HiloValidationState::ReviewYellow,
        }
    };
    let result = Rope::from_str(&compiled.result_text);
    let ranges = compiled
        .result_spans()
        .into_iter()
        .filter(|span| !span.is_empty())
        .map(|span| utf8_span_to_utf16(&result, span).map(|span| (span, state)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AcceptRefusal::Invalid(error.to_string()))?;
    Ok(AcceptPlan {
        file_path: path.to_string(),
        base_content_hash: compiled.base_content_hash.clone(),
        changes: compiled
            .edits
            .iter()
            .map(|edit| TextChange {
                range_utf16: edit.span_utf16,
                range_utf8: Some(edit.span_utf8),
                inserted_text: edit.replacement.clone(),
            })
            .collect(),
        result_content_hash: compiled.result_content_hash,
        ranges,
    })
}

fn refusal(error: CompileError, path: &str, current: &Rope, file: &ReviewFile) -> AcceptRefusal {
    if error.is_conflict() {
        AcceptRefusal::Conflict {
            path: path.to_string(),
            reason: error.to_string(),
            current_text: current.to_string(),
            proposed_text: file.proposed_text.clone(),
        }
    } else {
        AcceptRefusal::Invalid(error.to_string())
    }
}

/// The text `changes` produce when applied one after another (host-core's order: each change
/// in the coordinates left by the previous ones).
pub fn apply_sequential(
    rope: &Rope,
    changes: &[TextChange],
) -> Result<Rope, OffsetConversionError> {
    let mut staged = rope.clone();
    for change in changes {
        let start = utf16_to_char_idx(&staged, change.range_utf16.start)?;
        let end = utf16_to_char_idx(&staged, change.range_utf16.end)?;
        if start > end {
            return Err(OffsetConversionError::InvalidSpanOrder {
                start: change.range_utf16.start.as_u32(),
                end: change.range_utf16.end.as_u32(),
            });
        }
        staged.remove(start..end);
        staged.insert(start, &change.inserted_text);
    }
    Ok(staged)
}

/// Whether applying `changes` (sequentially) to `rope` gives exactly the planned content.
#[must_use]
pub fn matches_plan(plan: &AcceptPlan, rope: &Rope, changes: &[TextChange]) -> bool {
    compute_rope_blake3_hex(rope) == plan.base_content_hash
        && apply_sequential(rope, changes)
            .is_ok_and(|result| compute_rope_blake3_hex(&result) == plan.result_content_hash)
}

/// The plan's changes in host-core's sequential order (what an `EditorTransaction` carries).
#[must_use]
pub fn sequential_changes(plan: &AcceptPlan) -> Vec<TextChange> {
    let mut shift: i64 = 0;
    plan.changes
        .iter()
        .map(|change| {
            let moved = |offset: u32| {
                host_core::Utf16CodeUnitOffset::new((i64::from(offset) + shift) as u32)
            };
            let next = TextChange {
                range_utf16: Utf16Span::new(
                    moved(change.range_utf16.start.as_u32()),
                    moved(change.range_utf16.end.as_u32()),
                ),
                range_utf8: None,
                inserted_text: change.inserted_text.clone(),
            };
            shift += change.inserted_text.encode_utf16().count() as i64
                - i64::from(change.range_utf16.len_code_units());
            next
        })
        .collect()
}

/// What `[Request Adjustment]` sends to the agent: the user's words, then the evidence.
#[must_use]
pub fn adjustment_prompt(note: &str, change: &JudgedChange) -> String {
    let evidence = &change.evidence.evidence;
    let mut prompt = format!(
        "The user reviewed your proposed change and asks for an adjustment. Nothing was applied.\n\nUser's request:\n{}\n\nJudge verdict on your last proposal: {}.\n",
        note.trim(),
        evidence.verdict.label_en()
    );
    for item in evidence.diagnostics.items.iter().take(20) {
        prompt.push_str(&format!(
            "- {} [{}] {}\n",
            item.file_path,
            item.code,
            item.message.lines().next().unwrap_or_default()
        ));
    }
    prompt
}
