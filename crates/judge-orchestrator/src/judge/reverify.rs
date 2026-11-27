//! Background re-verification of `🟠 Stale Validation` ranges (`LOT-015`).
//!
//! Editing a `🟢` range makes it `🟠` synchronously (host-core). The Judge then checks the new
//! content again, but not on every keystroke: once the document has been idle for
//! [`REVERIFY_IDLE`], or right away on an explicit save.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use host_core::{DocumentValidationRange, Evidence, HiloValidationState, Utf16Span};

/// How long a document must stay unedited before its stale ranges are verified again.
pub const REVERIFY_IDLE: Duration = Duration::from_millis(1500);

/// Which documents have stale ranges to verify, and since when they are idle.
#[derive(Debug, Default)]
pub struct StaleReverifier {
    pending: BTreeMap<String, Instant>,
}

impl StaleReverifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An edit made `document` stale (or kept it so): its countdown starts again.
    pub fn note_edit(&mut self, document: &str, now: Instant) {
        self.pending
            .insert(document.to_string(), now + REVERIFY_IDLE);
    }

    /// An explicit save: verify without waiting.
    pub fn note_save(&mut self, document: &str, now: Instant) {
        if let Some(due) = self.pending.get_mut(document) {
            *due = now;
        }
    }

    /// Content that was not the verified one (a rebased change): verify it without waiting.
    pub fn verify_now(&mut self, document: &str, now: Instant) {
        self.pending.insert(document.to_string(), now);
    }

    pub fn forget(&mut self, document: &str) {
        self.pending.remove(document);
    }

    /// Documents whose idle time has run out, removed from the queue.
    pub fn take_due(&mut self, now: Instant) -> Vec<String> {
        let due: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(document, _)| document.clone())
            .collect();
        for document in &due {
            self.pending.remove(document);
        }
        due
    }

    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.values().min().copied()
    }
}

/// New states for `ranges` after the Judge verified `path` again. `None` when the document no
/// longer has the content that was verified (it was edited meanwhile): the result is dropped
/// and the ranges stay `🟠`.
#[must_use]
pub fn reverified_ranges(
    ranges: &[DocumentValidationRange],
    evidence: &Evidence,
    path: &str,
    current_content_hash: &str,
) -> Option<Vec<(Utf16Span, HiloValidationState)>> {
    if evidence.tested_file_hash(path) != Some(current_content_hash) {
        return None;
    }
    let verified = if evidence.satisfies_validated_green_for_file(path, current_content_hash) {
        HiloValidationState::ValidatedGreen
    } else if evidence.verdict == HiloValidationState::FailedRed {
        HiloValidationState::FailedRed
    } else {
        HiloValidationState::ReviewYellow
    };
    Some(
        ranges
            .iter()
            .map(|range| {
                let state = if range.state == HiloValidationState::StaleOrange {
                    verified
                } else {
                    range.state
                };
                (range.span_utf16, state)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_is_due_only_after_staying_idle_and_at_once_on_save() {
        let mut reverifier = StaleReverifier::new();
        let t0 = Instant::now();
        reverifier.note_edit("a.ts", t0);
        reverifier.note_edit("a.ts", t0 + Duration::from_millis(1000));
        assert!(reverifier
            .take_due(t0 + Duration::from_millis(2000))
            .is_empty());
        assert_eq!(
            reverifier.take_due(t0 + Duration::from_millis(2500)),
            ["a.ts"]
        );
        assert!(reverifier.take_due(t0 + Duration::from_secs(10)).is_empty());

        reverifier.note_edit("b.ts", t0);
        reverifier.note_save("b.ts", t0 + Duration::from_millis(10));
        assert_eq!(
            reverifier.take_due(t0 + Duration::from_millis(10)),
            ["b.ts"]
        );
        // Saving a document without stale ranges schedules nothing.
        reverifier.note_save("c.ts", t0);
        assert_eq!(reverifier.next_due(), None);
    }
}
