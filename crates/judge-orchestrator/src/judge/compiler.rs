//! `AstChangeSetCompiler` (`LOT-015`): the operations of a `ChangeSet` become exact edits on an
//! immutable snapshot of each file.
//!
//! An operation names its target by symbol and carries the BLAKE3 hash of what it expects to
//! find there. The compiler looks the symbol up in the **current** text, so an operation still
//! lands on its code when unrelated edits moved it (`Anchoring::Relocated`); when the target
//! itself changed, the hash no longer matches and the operation is a conflict. It is never
//! applied at the offsets the agent saw, and never over a change the user made.

use std::collections::BTreeMap;

use context_engine::{IncrementalAstParser, RopeTextProvider, SupportedAstLanguage, SyntaxTree};
use host_core::{
    ropey::Rope, utf8_span_to_utf16, ChangeOperation, ChangeSet, Utf16Span, Utf8ByteOffset,
    Utf8Span, CONTRACT_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::blocks::{resolve_symbol, BlockLookup};

/// One edit of the snapshot, in the coordinates of the text it was compiled against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledEdit {
    /// Index of the operation in `ChangeSet::operations`.
    pub operation: usize,
    pub span_utf8: Utf8Span,
    pub span_utf16: Utf16Span,
    pub replacement: String,
}

/// Whether every operation found its target where the agent saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Anchoring {
    Exact,
    /// Unrelated edits moved a target (or changed its surroundings); its content is unchanged.
    Relocated,
}

/// The compiled operations of one file and the text they produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledFile {
    pub file_path: String,
    /// BLAKE3 of the text the edits apply to.
    pub base_content_hash: String,
    /// Ascending and disjoint.
    pub edits: Vec<CompiledEdit>,
    pub anchoring: Anchoring,
    pub result_text: String,
    pub result_content_hash: String,
}

impl CompiledFile {
    /// UTF-8 spans of the replacements in `result_text`, in edit order.
    #[must_use]
    pub fn result_spans(&self) -> Vec<Utf8Span> {
        let mut delta: i64 = 0;
        self.edits
            .iter()
            .map(|edit| {
                let start = (i64::from(edit.span_utf8.start.as_u32()) + delta) as u32;
                let end = start + edit.replacement.len() as u32;
                delta += edit.replacement.len() as i64 - i64::from(edit.span_utf8.len_bytes());
                Utf8Span::new(Utf8ByteOffset::new(start), Utf8ByteOffset::new(end))
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledChangeSet {
    pub changeset_id: String,
    pub task_id: String,
    pub base_revision: u32,
    /// Sorted by path.
    pub files: Vec<CompiledFile>,
    /// BLAKE3 over every `(path, result_content_hash)`: what the Judge tests is exactly this.
    pub result_digest: String,
}

impl CompiledChangeSet {
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&CompiledFile> {
        self.files.iter().find(|file| file.file_path == path)
    }

    #[must_use]
    pub fn anchoring(&self) -> Anchoring {
        if self
            .files
            .iter()
            .all(|file| file.anchoring == Anchoring::Exact)
        {
            Anchoring::Exact
        } else {
            Anchoring::Relocated
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum CompileError {
    #[error("unsupported ChangeSet schema version {0}")]
    UnsupportedSchema(u16),
    #[error("the ChangeSet has no operation")]
    Empty,
    #[error("{file}: not listed in affected_files")]
    UndeclaredFile { file: String },
    #[error("{file}: no text is available for this file")]
    MissingFile { file: String },
    #[error("{file}: operation {operation} targets `{symbol}`, which is not in the current text")]
    SymbolNotFound {
        file: String,
        operation: usize,
        symbol: String,
    },
    #[error("{file}: operation {operation} targets `{symbol}`, declared more than once")]
    AmbiguousSymbol {
        file: String,
        operation: usize,
        symbol: String,
    },
    #[error("{file}: operation {operation} expects a {expected} for `{symbol}`, found {actual}")]
    KindMismatch {
        file: String,
        operation: usize,
        symbol: String,
        expected: String,
        actual: String,
    },
    #[error("{file}: `{symbol}` changed since operation {operation} was proposed")]
    TargetChanged {
        file: String,
        operation: usize,
        symbol: String,
    },
    #[error("{file}: the code operation {operation} replaces is no longer in the text")]
    SearchBlockNotFound { file: String, operation: usize },
    #[error("{file}: the code operation {operation} replaces appears more than once")]
    SearchBlockAmbiguous { file: String, operation: usize },
    #[error("{file}: operations {first} and {second} overlap")]
    OverlappingOperations {
        file: String,
        first: usize,
        second: usize,
    },
    #[error("{file}: {reason}")]
    Offset { file: String, reason: String },
}

impl CompileError {
    /// The target was edited since the proposal: the change must be reviewed against the new
    /// text, never applied.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            Self::TargetChanged { .. }
                | Self::SearchBlockNotFound { .. }
                | Self::SymbolNotFound { .. }
        )
    }
}

pub struct AstChangeSetCompiler;

impl AstChangeSetCompiler {
    /// Compiles the operations of `changeset` that target `file_path` against `rope`, whose
    /// syntax tree is `tree`.
    pub fn compile(
        rope: &Rope,
        tree: &SyntaxTree,
        changeset: &ChangeSet,
        file_path: &str,
    ) -> Result<CompiledFile, CompileError> {
        let text = rope.to_string();
        let mut anchoring = Anchoring::Exact;
        let mut edits = Vec::new();
        for (index, operation) in changeset.operations.iter().enumerate() {
            if operation_path(operation) != file_path {
                continue;
            }
            let (span, replacement, relocated) =
                resolve_operation(rope, tree, &text, file_path, index, operation)?;
            if relocated {
                anchoring = Anchoring::Relocated;
            }
            let span_utf16 =
                utf8_span_to_utf16(rope, span).map_err(|error| CompileError::Offset {
                    file: file_path.to_string(),
                    reason: error.to_string(),
                })?;
            edits.push(CompiledEdit {
                operation: index,
                span_utf8: span,
                span_utf16,
                replacement,
            });
        }
        edits.sort_by_key(|edit| (edit.span_utf8.start, edit.span_utf8.end));
        for pair in edits.windows(2) {
            if pair[0].span_utf8.end > pair[1].span_utf8.start
                || pair[0].span_utf8 == pair[1].span_utf8
            {
                return Err(CompileError::OverlappingOperations {
                    file: file_path.to_string(),
                    first: pair[0].operation.min(pair[1].operation),
                    second: pair[0].operation.max(pair[1].operation),
                });
            }
        }
        let mut result = String::with_capacity(text.len());
        let mut cursor = 0;
        for edit in &edits {
            result.push_str(&text[cursor..edit.span_utf8.start.as_usize()]);
            result.push_str(&edit.replacement);
            cursor = edit.span_utf8.end.as_usize();
        }
        result.push_str(&text[cursor..]);
        Ok(CompiledFile {
            file_path: file_path.to_string(),
            base_content_hash: hash(&text),
            edits,
            anchoring,
            result_content_hash: hash(&result),
            result_text: result,
        })
    }

    /// Compiles every file of `changeset`; `source` gives the current text of a file.
    pub fn compile_changeset(
        changeset: &ChangeSet,
        source: impl Fn(&str) -> Option<Rope>,
    ) -> Result<CompiledChangeSet, CompileError> {
        if changeset.schema_version != CONTRACT_SCHEMA_VERSION {
            return Err(CompileError::UnsupportedSchema(changeset.schema_version));
        }
        if changeset.operations.is_empty() {
            return Err(CompileError::Empty);
        }
        let mut paths: BTreeMap<&str, ()> = BTreeMap::new();
        for operation in &changeset.operations {
            let path = operation_path(operation);
            if !changeset.affected_files.iter().any(|file| file == path) {
                return Err(CompileError::UndeclaredFile {
                    file: path.to_string(),
                });
            }
            paths.insert(path, ());
        }
        let mut files = Vec::with_capacity(paths.len());
        for path in paths.keys() {
            let rope = source(path).ok_or_else(|| CompileError::MissingFile {
                file: (*path).to_string(),
            })?;
            let tree = parse(&rope, path);
            files.push(Self::compile(&rope, &tree, changeset, path)?);
        }
        let result_digest = result_digest(
            files
                .iter()
                .map(|file| (file.file_path.as_str(), file.result_content_hash.as_str())),
        );
        Ok(CompiledChangeSet {
            changeset_id: changeset.changeset_id.clone(),
            task_id: changeset.task_id.clone(),
            base_revision: changeset.base_revision,
            files,
            result_digest,
        })
    }
}

/// The digest of a set of files, independent of their order.
pub fn result_digest<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut sorted: Vec<_> = files.into_iter().collect();
    sorted.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hilo.changeset.result.v1\0");
    for (path, content_hash) in sorted {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(content_hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

pub fn parse(rope: &Rope, path: &str) -> SyntaxTree {
    IncrementalAstParser::new().parse_with(
        SupportedAstLanguage::from_path(path),
        &RopeTextProvider::new(rope),
        None,
    )
}

pub(crate) fn hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

pub(crate) fn operation_path(operation: &ChangeOperation) -> &str {
    match operation {
        ChangeOperation::AstNodeReplace { file_path, .. }
        | ChangeOperation::SymbolSearchReplace { file_path, .. } => file_path,
    }
}

/// Where operation `index` applies in `text`, what it writes there, and whether it moved.
fn resolve_operation(
    rope: &Rope,
    tree: &SyntaxTree,
    text: &str,
    file: &str,
    index: usize,
    operation: &ChangeOperation,
) -> Result<(Utf8Span, String, bool), CompileError> {
    let lookup = |symbol: &str, hint: Option<u32>| match resolve_symbol(rope, tree, symbol, hint) {
        BlockLookup::Found(block) => Ok(block),
        BlockLookup::NotFound => Err(CompileError::SymbolNotFound {
            file: file.to_string(),
            operation: index,
            symbol: symbol.to_string(),
        }),
        BlockLookup::Ambiguous => Err(CompileError::AmbiguousSymbol {
            file: file.to_string(),
            operation: index,
            symbol: symbol.to_string(),
        }),
    };
    match operation {
        ChangeOperation::AstNodeReplace {
            symbol_fqn,
            ast_node_kind,
            expected_span_utf8,
            expected_block_blake3,
            replacement_code,
            ..
        } => {
            let block = lookup(symbol_fqn, Some(expected_span_utf8.start.as_u32()))?;
            if let Some(kind) = block.kind {
                if !ast_node_kind.is_empty() && kind.sexp_tag() != ast_node_kind {
                    return Err(CompileError::KindMismatch {
                        file: file.to_string(),
                        operation: index,
                        symbol: symbol_fqn.clone(),
                        expected: ast_node_kind.clone(),
                        actual: kind.sexp_tag().to_string(),
                    });
                }
            }
            let current = slice(text, block.span_utf8);
            if hash(current) != *expected_block_blake3 {
                return Err(CompileError::TargetChanged {
                    file: file.to_string(),
                    operation: index,
                    symbol: symbol_fqn.clone(),
                });
            }
            let mut replacement = replacement_code.clone();
            // A block ends with its line break: keep the next line where it was.
            if current.ends_with('\n') && !replacement.is_empty() && !replacement.ends_with('\n') {
                replacement.push('\n');
            }
            Ok((
                block.span_utf8,
                replacement,
                block.span_utf8 != *expected_span_utf8,
            ))
        }
        ChangeOperation::SymbolSearchReplace {
            anchor_symbol,
            search_block,
            replace_block,
            expected_before_blake3,
            ..
        } => {
            let scope = if anchor_symbol.is_empty() {
                Utf8Span::new(Utf8ByteOffset::ZERO, Utf8ByteOffset::new(text.len() as u32))
            } else {
                lookup(anchor_symbol, None)?.span_utf8
            };
            let scope_text = slice(text, scope);
            let unchanged_scope = hash(scope_text) == *expected_before_blake3;
            if search_block.is_empty() {
                // Only an empty scope can be replaced without a search block.
                if !scope_text.is_empty() {
                    return Err(CompileError::SearchBlockNotFound {
                        file: file.to_string(),
                        operation: index,
                    });
                }
                return Ok((scope, replace_block.clone(), false));
            }
            let mut matches = scope_text.match_indices(search_block.as_str());
            let Some((found, _)) = matches.next() else {
                return Err(CompileError::SearchBlockNotFound {
                    file: file.to_string(),
                    operation: index,
                });
            };
            if matches.next().is_some() {
                return Err(CompileError::SearchBlockAmbiguous {
                    file: file.to_string(),
                    operation: index,
                });
            }
            let start = scope.start.as_u32() + found as u32;
            Ok((
                Utf8Span::new(
                    Utf8ByteOffset::new(start),
                    Utf8ByteOffset::new(start + search_block.len() as u32),
                ),
                replace_block.clone(),
                !unchanged_scope,
            ))
        }
    }
}

fn slice(text: &str, span: Utf8Span) -> &str {
    &text[span.start.as_usize()..span.end.as_usize()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "import { round } from \"./round\";\n\nexport function add(a: number, b: number): number {\n  return a + b;\n}\n\nexport function sub(a: number, b: number): number {\n  return a - b;\n}\n";

    fn block_of(source: &str, symbol: &str) -> (Utf8Span, String) {
        let rope = Rope::from_str(source);
        let BlockLookup::Found(block) = resolve_symbol(&rope, &parse(&rope, "a.ts"), symbol, None)
        else {
            panic!("{symbol}")
        };
        (block.span_utf8, hash(slice(source, block.span_utf8)))
    }

    fn replace(source: &str, symbol: &str, code: &str) -> ChangeOperation {
        let (span, digest) = block_of(source, symbol);
        ChangeOperation::AstNodeReplace {
            file_path: "src/money.ts".into(),
            symbol_fqn: symbol.into(),
            ast_node_kind: "function_declaration".into(),
            expected_span_utf8: span,
            expected_block_blake3: digest,
            replacement_code: code.into(),
        }
    }

    fn changeset(operations: Vec<ChangeOperation>) -> ChangeSet {
        ChangeSet {
            schema_version: CONTRACT_SCHEMA_VERSION,
            changeset_id: "cs-1".into(),
            task_id: "task-1".into(),
            base_revision: 0,
            operations,
            affected_files: vec!["src/money.ts".into()],
            result_digest: String::new(),
        }
    }

    fn compile(source: &str, cs: &ChangeSet) -> Result<CompiledChangeSet, CompileError> {
        AstChangeSetCompiler::compile_changeset(cs, |path| {
            (path == "src/money.ts").then(|| Rope::from_str(source))
        })
    }

    #[test]
    fn an_ast_replacement_rewrites_exactly_the_symbol_block_with_both_offset_units() {
        let cs = changeset(vec![replace(
            SOURCE,
            "add",
            "export function add(a: number, b: number): number {\n  return round(a + b); // é𝄞\n}",
        )]);
        let compiled = compile(SOURCE, &cs).unwrap();
        let file = &compiled.files[0];
        assert_eq!(file.anchoring, Anchoring::Exact);
        assert_eq!(
            file.result_text,
            SOURCE.replace("  return a + b;\n", "  return round(a + b); // é𝄞\n")
        );
        let edit = &file.edits[0];
        assert_eq!(
            &SOURCE[edit.span_utf8.start.as_usize()..edit.span_utf8.end.as_usize()],
            "export function add(a: number, b: number): number {\n  return a + b;\n}\n"
        );
        // ASCII before the edit: both units agree on its start; the span lengths agree too.
        assert_eq!(
            edit.span_utf16.start.as_u32(),
            edit.span_utf8.start.as_u32()
        );
        assert_eq!(
            compiled.result_digest,
            result_digest([("src/money.ts", file.result_content_hash.as_str())])
        );
        let result_span = file.result_spans()[0];
        assert!(
            file.result_text[result_span.start.as_usize()..result_span.end.as_usize()]
                .contains("é𝄞")
        );
    }

    #[test]
    fn an_unrelated_edit_above_relocates_the_target_instead_of_using_stale_offsets() {
        let cs = changeset(vec![replace(
            SOURCE,
            "sub",
            "export function sub(a: number, b: number): number {\n  return b - a;\n}\n",
        )]);
        let edited = SOURCE.replace("import {", "// a comment the user typed 🦀\nimport {");
        let compiled = compile(&edited, &cs).unwrap();
        let file = &compiled.files[0];
        assert_eq!(file.anchoring, Anchoring::Relocated);
        assert_eq!(
            file.result_text,
            edited.replace("return a - b;", "return b - a;")
        );
        assert!(file
            .result_text
            .starts_with("// a comment the user typed 🦀\n"));
        // The offsets moved by the inserted line; UTF-16 counts the crab as two units, UTF-8 as four.
        let edit = &file.edits[0];
        let expected = changeset_span(&cs);
        assert!(edit.span_utf8.start > expected.start);
        assert_eq!(
            edit.span_utf8.start.as_u32() - edit.span_utf16.start.as_u32(),
            2
        );
    }

    fn changeset_span(cs: &ChangeSet) -> Utf8Span {
        match &cs.operations[0] {
            ChangeOperation::AstNodeReplace {
                expected_span_utf8, ..
            } => *expected_span_utf8,
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_user_edit_inside_the_target_is_a_conflict_never_an_overwrite() {
        let cs = changeset(vec![replace(SOURCE, "add", "export function add() {}\n")]);
        let edited = SOURCE.replace("return a + b;", "return a + b + 0; // mine");
        let error = compile(&edited, &cs).unwrap_err();
        assert!(error.is_conflict(), "{error}");
        assert!(matches!(error, CompileError::TargetChanged { .. }));
    }

    #[test]
    fn search_replace_finds_its_block_once_inside_the_anchor_and_reports_rebases() {
        let (_, sub_hash) = block_of(SOURCE, "sub");
        let op = |search: &str, before: &str| ChangeOperation::SymbolSearchReplace {
            file_path: "src/money.ts".into(),
            anchor_symbol: "sub".into(),
            search_block: search.into(),
            replace_block: "  return (a - b) | 0;\n".into(),
            expected_before_blake3: before.into(),
        };
        let compiled =
            compile(SOURCE, &changeset(vec![op("  return a - b;\n", &sub_hash)])).unwrap();
        assert_eq!(compiled.files[0].anchoring, Anchoring::Exact);
        assert!(compiled.files[0]
            .result_text
            .contains("return (a - b) | 0;"));
        assert!(compiled.files[0].result_text.contains("return a + b;"));

        // The user added a line to `sub` elsewhere: the searched lines are intact, so the
        // operation applies, and is reported as rebased.
        let edited = SOURCE.replace("  return a - b;\n", "  // why\n  return a - b;\n");
        let rebased = compile(
            &edited,
            &changeset(vec![op("  return a - b;\n", &sub_hash)]),
        )
        .unwrap();
        assert_eq!(rebased.files[0].anchoring, Anchoring::Relocated);
        assert!(rebased.files[0]
            .result_text
            .contains("  // why\n  return (a - b) | 0;\n"));

        let gone = SOURCE.replace("return a - b;", "return b - a;");
        assert!(
            compile(&gone, &changeset(vec![op("  return a - b;\n", &sub_hash)]))
                .unwrap_err()
                .is_conflict()
        );
        // `return a` appears in both functions but the anchor keeps the search to `sub`.
        assert!(compile(SOURCE, &changeset(vec![op("return a", &sub_hash)])).is_ok());
        let file_wide = ChangeOperation::SymbolSearchReplace {
            file_path: "src/money.ts".into(),
            anchor_symbol: String::new(),
            search_block: "return a".into(),
            replace_block: "return 0".into(),
            expected_before_blake3: hash(SOURCE),
        };
        assert!(matches!(
            compile(SOURCE, &changeset(vec![file_wide])).unwrap_err(),
            CompileError::SearchBlockAmbiguous { .. }
        ));
    }

    #[test]
    fn overlapping_operations_and_undeclared_files_are_refused() {
        let first = replace(SOURCE, "add", "export function add() {}\n");
        let (_, add_hash) = block_of(SOURCE, "add");
        let inside = ChangeOperation::SymbolSearchReplace {
            file_path: "src/money.ts".into(),
            anchor_symbol: "add".into(),
            search_block: "return a + b;".into(),
            replace_block: "return 1;".into(),
            expected_before_blake3: add_hash,
        };
        assert!(matches!(
            compile(SOURCE, &changeset(vec![first.clone(), inside])).unwrap_err(),
            CompileError::OverlappingOperations {
                first: 0,
                second: 1,
                ..
            }
        ));
        let mut undeclared = changeset(vec![first]);
        undeclared.affected_files.clear();
        assert!(matches!(
            compile(SOURCE, &undeclared).unwrap_err(),
            CompileError::UndeclaredFile { .. }
        ));
        let mut wrong_kind = changeset(vec![replace(SOURCE, "add", "x")]);
        if let ChangeOperation::AstNodeReplace { ast_node_kind, .. } = &mut wrong_kind.operations[0]
        {
            *ast_node_kind = "class_declaration".into();
        }
        assert!(matches!(
            compile(SOURCE, &wrong_kind).unwrap_err(),
            CompileError::KindMismatch { .. }
        ));
    }
}
