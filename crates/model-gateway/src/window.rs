//! How much text around the cursor a completion sends, measured on a local 1.5B model.
//!
//! Prompt evaluation dominates latency: on an M-series Mac about 1 ms per prompt token cold,
//! 11 ms per generated token. Ollama reuses the evaluated part of a prompt that starts like the
//! previous one, so the window's start is anchored: it moves in steps of [`ANCHOR_CHARS`], not
//! with every keystroke, and typing only re-evaluates what follows the cursor.

/// Characters before the cursor (up to `PREFIX_CHARS + ANCHOR_CHARS - 1` with the anchor).
pub const PREFIX_CHARS: usize = 2000;
/// Characters after the cursor. Re-evaluated on every keystroke, so kept short.
pub const SUFFIX_CHARS: usize = 300;
pub const ANCHOR_CHARS: usize = 512;
/// A suggestion is a line or a few: 32 tokens keep generation near 350 ms.
pub const MAX_OUTPUT_TOKENS: u32 = 32;

/// `[start, cursor)` and `[cursor, end)` as character indexes in a text of `len` characters.
pub fn context_bounds(cursor: usize, len: usize) -> (usize, usize) {
    let cursor = cursor.min(len);
    let start = cursor.saturating_sub(PREFIX_CHARS) / ANCHOR_CHARS * ANCHOR_CHARS;
    (start, (cursor + SUFFIX_CHARS).min(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_stays_put_while_typing_so_the_runtime_can_reuse_the_prompt() {
        // At an anchor step, the start holds for the next ANCHOR_CHARS - 1 characters typed.
        let cursor = PREFIX_CHARS + 5 * ANCHOR_CHARS;
        let (start, _) = context_bounds(cursor, 10_000);
        for typed in 1..ANCHOR_CHARS {
            assert_eq!(context_bounds(cursor + typed, 10_000 + typed).0, start);
        }
        assert_eq!(
            context_bounds(cursor + ANCHOR_CHARS, 10_600).0,
            start + ANCHOR_CHARS
        );
        // Wherever the cursor is, at least PREFIX_CHARS are sent before it.
        for cursor in PREFIX_CHARS..PREFIX_CHARS + 3 * ANCHOR_CHARS {
            let sent = cursor - context_bounds(cursor, 10_000).0;
            assert!((PREFIX_CHARS..PREFIX_CHARS + ANCHOR_CHARS).contains(&sent));
        }
    }

    #[test]
    fn bounds_stay_inside_the_text() {
        assert_eq!(context_bounds(10, 50), (0, 50));
        assert_eq!(context_bounds(80, 50), (0, 50));
        assert_eq!(context_bounds(0, 0), (0, 0));
        assert_eq!(context_bounds(3000, 3100).1, 3100);
    }
}
