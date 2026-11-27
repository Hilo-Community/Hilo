//! Which tests a change can break: the symbols whose blocks it touches, through the `LOT-007`
//! dependency graph (Test Impact Analysis).

use std::collections::BTreeSet;

use context_engine::{is_test_file_path, ImpactAnalyzer};
use host_core::{ropey::Rope, Utf8Span};

use super::blocks::block_end;
use super::compiler::parse;

/// Hops followed from a changed symbol to the tests that use it.
pub const IMPACT_DEPTH: u32 = 4;

/// Names of the symbols of `text` whose block intersects one of `spans`.
#[must_use]
pub fn touched_symbols(path: &str, text: &str, spans: &[Utf8Span]) -> Vec<String> {
    let rope = Rope::from_str(text);
    let tree = parse(&rope, path);
    let mut names = BTreeSet::new();
    for symbol in tree.symbols() {
        let start = symbol.span_utf8.start.as_usize();
        let end = block_end(text, start);
        let touched = spans.iter().any(|span| {
            let (from, to) = (span.start.as_usize(), span.end.as_usize());
            from < end && start < to.max(from + 1)
        });
        if touched {
            names.insert(symbol.name);
        }
    }
    names.into_iter().collect()
}

/// The impacted test files for changes to `files` (path, changed text, changed spans), sorted
/// by distance. A changed test file is always included.
#[must_use]
pub fn impacted_tests(
    analyzer: &ImpactAnalyzer,
    files: &[(&str, &str, Vec<Utf8Span>)],
) -> Vec<String> {
    let mut tests = BTreeSet::new();
    let mut ordered = Vec::new();
    let mut touched: Vec<(String, String)> = Vec::new();
    for (path, text, spans) in files {
        if is_test_file_path(path) && tests.insert((*path).to_string()) {
            ordered.push((*path).to_string());
        }
        for symbol in touched_symbols(path, text, spans) {
            touched.push(((*path).to_string(), symbol));
        }
    }
    let pairs: Vec<(&str, &str)> = touched
        .iter()
        .map(|(path, symbol)| (path.as_str(), symbol.as_str()))
        .collect();
    for target in analyzer.find_impacted_tests(&pairs, IMPACT_DEPTH) {
        if tests.insert(target.test_file_path.clone()) {
            ordered.push(target.test_file_path);
        }
    }
    ordered
}
