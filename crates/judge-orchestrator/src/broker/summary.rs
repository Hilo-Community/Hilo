//! The summary of an outcome is journaled; a tool's output can be far larger than a journal
//! record may be. Keep its beginning and end, and a digest of the whole.

use crate::IntentOutcome;

/// Largest summary kept in the journal, in bytes.
pub(crate) const MAX_SUMMARY_BYTES: usize = 64 * 1024;
const KEEP: usize = MAX_SUMMARY_BYTES / 2 - 256;

fn floor_boundary(text: &str, index: usize) -> usize {
    (0..=index.min(text.len()))
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

fn ceil_boundary(text: &str, index: usize) -> usize {
    (index.min(text.len())..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len())
}

/// `outcome` with its summary cut to at most [`MAX_SUMMARY_BYTES`]: the first and last parts,
/// how much was left out, and the BLAKE3 of the full text so it can still be matched.
pub(super) fn bounded(outcome: IntentOutcome) -> IntentOutcome {
    let text = &outcome.summary;
    if text.len() <= MAX_SUMMARY_BYTES {
        return outcome;
    }
    let head = floor_boundary(text, KEEP);
    let tail = ceil_boundary(text, text.len() - KEEP);
    let summary = format!(
        "{}\n[... {} bytes omitted; blake3 of the full output: {} ...]\n{}",
        &text[..head],
        tail - head,
        blake3::hash(text.as_bytes()).to_hex(),
        &text[tail..]
    );
    IntentOutcome {
        succeeded: outcome.succeeded,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(summary: String) -> IntentOutcome {
        IntentOutcome {
            succeeded: true,
            summary,
        }
    }

    #[test]
    fn small_summaries_are_kept_whole() {
        let text = "x".repeat(MAX_SUMMARY_BYTES);
        assert_eq!(bounded(outcome(text.clone())).summary, text);
    }

    #[test]
    fn large_summaries_keep_both_ends_and_a_digest_of_the_whole() {
        let text = format!("BEGIN{}END", "é".repeat(5_000_000));
        let cut = bounded(outcome(text.clone())).summary;
        assert!(cut.len() <= MAX_SUMMARY_BYTES, "{}", cut.len());
        assert!(cut.starts_with("BEGIN") && cut.ends_with("END"));
        assert!(cut.contains(blake3::hash(text.as_bytes()).to_hex().as_str()));
    }
}
