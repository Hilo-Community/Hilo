//! What the verification tools report, as `DiagnosticEntry`s anchored on UTF-16 spans of the
//! exact text that was checked. Every conversion goes through `host-core`'s converter.

use std::path::Path;

use host_core::{
    line_col_to_utf16_clamped, ropey::Rope, utf16_to_line_col, utf8_span_to_utf16, DiagnosticEntry,
    DiagnosticSeverity, DiagnosticSource, LineCol, Utf16CodeUnitOffset, Utf16Span, Utf8ByteOffset,
    Utf8Span,
};
use serde_json::Value;

/// Longest message kept per diagnostic: tool output is data, and bounded.
const MAX_MESSAGE_CHARS: usize = 600;
/// Most diagnostics kept per tool run.
pub const MAX_ITEMS_PER_RUN: usize = 64;

/// The text of a checked file (as written in the worktree), by workspace-relative path.
pub type TextOf<'a> = &'a dyn Fn(&str) -> Option<Rope>;

/// `biome check --reporter=json` output. Biome spans are UTF-8 byte offsets.
#[must_use]
pub fn parse_biome(stdout: &str, text_of: TextOf<'_>) -> Vec<DiagnosticEntry> {
    let Ok(report) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    let Some(diagnostics) = report["diagnostics"].as_array() else {
        return Vec::new();
    };
    diagnostics
        .iter()
        .take(MAX_ITEMS_PER_RUN)
        .map(|item| {
            let file = item["location"]["path"]["file"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let span = item["location"]["span"].as_array().and_then(|span| {
                Some(Utf8Span::new(
                    Utf8ByteOffset::new(span.first()?.as_u64()? as u32),
                    Utf8ByteOffset::new(span.get(1)?.as_u64()? as u32),
                ))
            });
            let (span_utf16, line_col) = match (span, text_of(&file)) {
                (Some(span), Some(rope)) => anchor_utf8(&rope, span),
                _ => (Utf16Span::default(), LineCol::default()),
            };
            DiagnosticEntry {
                file_path: file,
                span_utf16,
                line_col,
                source: DiagnosticSource::Biome,
                severity: severity(item["severity"].as_str().unwrap_or("error")),
                code: item["category"].as_str().unwrap_or("biome").to_string(),
                message: bounded(item["description"].as_str().unwrap_or_default()),
            }
        })
        .collect()
}

/// `tsc --noEmit --pretty false` output: `path(line,col): error TS1234: message`, 1-based, with
/// columns in UTF-16 code units (TypeScript indexes JavaScript strings).
#[must_use]
pub fn parse_tsc(output: &str, text_of: TextOf<'_>) -> Vec<DiagnosticEntry> {
    let mut items: Vec<DiagnosticEntry> = Vec::new();
    for line in output.lines() {
        if line.starts_with(' ') {
            if let Some(last) = items.last_mut() {
                last.message = bounded(&format!("{}\n{}", last.message, line.trim()));
            }
            continue;
        }
        let Some((location, rest)) = line.split_once("): ") else {
            continue;
        };
        let Some((file, position)) = location.rsplit_once('(') else {
            continue;
        };
        let Some((row, col)) = position.split_once(',') else {
            continue;
        };
        let (Ok(row), Ok(col)) = (row.parse::<u32>(), col.parse::<u32>()) else {
            continue;
        };
        let (kind, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let (code, message) = rest.split_once(": ").unwrap_or(("", rest));
        let line_col = LineCol::new(row.saturating_sub(1), col.saturating_sub(1));
        let start = text_of(file).map_or(Utf16CodeUnitOffset::ZERO, |rope| {
            line_col_to_utf16_clamped(&rope, line_col)
        });
        if items.len() == MAX_ITEMS_PER_RUN {
            break;
        }
        items.push(DiagnosticEntry {
            file_path: file.to_string(),
            span_utf16: Utf16Span::new(start, start),
            line_col,
            source: DiagnosticSource::Compiler,
            severity: severity(kind),
            code: code.to_string(),
            message: bounded(message),
        });
    }
    items
}

/// `vitest run --reporter=json` output: one entry per failed test, and per test file that
/// could not run at all. Paths in the report are absolute inside `workspace`.
#[must_use]
pub fn parse_vitest(stdout: &str, workspace: &Path) -> Vec<DiagnosticEntry> {
    let Some(report) = vitest_report(stdout) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for file in report["testResults"].as_array().into_iter().flatten() {
        let path = relative(file["name"].as_str().unwrap_or_default(), workspace);
        let mut failed_tests = 0;
        for test in file["assertionResults"].as_array().into_iter().flatten() {
            if test["status"] != "failed" {
                continue;
            }
            failed_tests += 1;
            let failure = test["failureMessages"]
                .as_array()
                .and_then(|messages| messages.first())
                .and_then(Value::as_str)
                .unwrap_or_default();
            items.push(test_failure(
                &path,
                &format!(
                    "{}: {}",
                    test["fullName"].as_str().unwrap_or_default(),
                    first_lines(failure, 6)
                ),
            ));
        }
        if file["status"] == "failed" && failed_tests == 0 {
            items.push(test_failure(
                &path,
                &first_lines(file["message"].as_str().unwrap_or("test file failed"), 8),
            ));
        }
    }
    items.truncate(MAX_ITEMS_PER_RUN);
    items
}

/// Total and failed test counts of a Vitest JSON report, if it parsed.
#[must_use]
pub fn vitest_counts(stdout: &str) -> Option<(u64, u64)> {
    let report = vitest_report(stdout)?;
    Some((
        report["numTotalTests"].as_u64()?,
        report["numFailedTests"].as_u64()?,
    ))
}

fn vitest_report(stdout: &str) -> Option<Value> {
    // The report is the JSON object on stdout; tests may print around it.
    let start = stdout.find("{\"numTotalTestSuites\"")?;
    serde_json::Deserializer::from_str(&stdout[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn test_failure(path: &str, message: &str) -> DiagnosticEntry {
    DiagnosticEntry {
        file_path: path.to_string(),
        span_utf16: Utf16Span::default(),
        line_col: LineCol::default(),
        source: DiagnosticSource::TestRunner,
        severity: DiagnosticSeverity::Error,
        code: "test-failed".into(),
        message: bounded(message),
    }
}

fn relative(path: &str, workspace: &Path) -> String {
    let path = Path::new(path);
    let canonical = workspace.canonicalize().ok();
    path.strip_prefix(workspace)
        .ok()
        .or_else(|| {
            canonical
                .as_deref()
                .and_then(|root| path.strip_prefix(root).ok())
        })
        .map_or_else(
            || path.to_string_lossy().into_owned(),
            |relative| relative.to_string_lossy().replace('\\', "/"),
        )
}

fn anchor_utf8(rope: &Rope, span: Utf8Span) -> (Utf16Span, LineCol) {
    let len = rope.len_bytes() as u32;
    let clamp = |offset: Utf8ByteOffset| Utf8ByteOffset::new(offset.as_u32().min(len));
    let span = Utf8Span::new(clamp(span.start), clamp(span.end));
    match utf8_span_to_utf16(rope, span) {
        Ok(span_utf16) => (
            span_utf16,
            utf16_to_line_col(rope, span_utf16.start).unwrap_or_default(),
        ),
        Err(_) => (Utf16Span::default(), LineCol::default()),
    }
}

fn severity(text: &str) -> DiagnosticSeverity {
    match text {
        "error" | "fatal" => DiagnosticSeverity::Error,
        "warning" => DiagnosticSeverity::Warning,
        _ => DiagnosticSeverity::Information,
    }
}

fn first_lines(text: &str, count: usize) -> String {
    text.lines().take(count).collect::<Vec<_>>().join("\n")
}

fn bounded(text: &str) -> String {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biome_byte_spans_become_utf16_spans_of_the_checked_text() {
        let text = "const é𝄞 = 1; let x = 2;\n";
        let start = text.find("let").unwrap();
        let report = format!(
            r#"{{"diagnostics":[{{"category":"lint/style/useConst","severity":"error","description":"Use const.","location":{{"path":{{"file":"src/a.ts"}},"span":[{start},{}]}}}}]}}"#,
            start + 3
        );
        let rope = Rope::from_str(text);
        let items = parse_biome(&report, &|path| (path == "src/a.ts").then(|| rope.clone()));
        assert_eq!(items.len(), 1);
        let item = &items[0];
        // `é` is 2 bytes / 1 unit, `𝄞` 4 bytes / 2 units: 3 bytes more than units before `let`.
        assert_eq!(item.span_utf16.start.as_u32(), start as u32 - 3);
        assert_eq!(item.span_utf16.len_code_units(), 3);
        assert_eq!(item.line_col, LineCol::new(0, start as u32 - 3));
        assert_eq!(item.code, "lint/style/useConst");
        assert_eq!(item.severity, DiagnosticSeverity::Error);
    }

    #[test]
    fn tsc_lines_keep_their_continuations_and_utf16_columns() {
        let text = "// 𝄞\nconst a: number = \"x\";\n";
        let rope = Rope::from_str(text);
        let output = "src/a.ts(2,7): error TS2322: Type 'string' is not assignable to type 'number'.\n  Details here.\nnot a diagnostic\n";
        let items = parse_tsc(output, &|_| Some(rope.clone()));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].code, "TS2322");
        assert_eq!(items[0].line_col, LineCol::new(1, 6));
        // Line 2 starts after "// 𝄞\n": 3 + 2 + 1 = 6 UTF-16 units.
        assert_eq!(items[0].span_utf16.start.as_u32(), 6 + 6);
        assert!(items[0].message.ends_with("Details here."));
    }

    #[test]
    fn vitest_failures_name_the_test_and_its_message() {
        let report = r#"noise {"numTotalTestSuites":1,"numTotalTests":2,"numFailedTests":1,"testResults":[{"name":"/w/src/money.test.ts","status":"failed","message":"","assertionResults":[{"fullName":"adds","status":"failed","failureMessages":["AssertionError: expected 4 to be 3\n    at stack"]},{"fullName":"ok","status":"passed","failureMessages":[]}]},{"name":"/w/src/broken.test.ts","status":"failed","message":"SyntaxError: Unexpected token","assertionResults":[]}]} trailing"#;
        let items = parse_vitest(report, Path::new("/w"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].file_path, "src/money.test.ts");
        assert!(items[0]
            .message
            .starts_with("adds: AssertionError: expected 4 to be 3"));
        assert_eq!(items[1].message, "SyntaxError: Unexpected token");
        assert_eq!(vitest_counts(report), Some((2, 1)));
    }
}
