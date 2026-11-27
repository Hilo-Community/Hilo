//! Where a symbol's code starts and ends.
//!
//! The `LOT-007` syntax tree is line-granular: a declaration node covers its first line only.
//! A `ChangeSet` replaces whole symbols, so the block of a declaration is computed here by
//! scanning from its line, outside strings, template literals and comments, until its brackets
//! are balanced again and the statement does not continue on the next line.

use context_engine::{SymbolKind, SyntaxTree};
use host_core::{ropey::Rope, Utf8ByteOffset, Utf8Span};

/// A resolved symbol: its block in the text and what the syntax tree says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolBlock {
    pub span_utf8: Utf8Span,
    /// `None` for a class member, which the line-granular tree does not classify.
    pub kind: Option<SymbolKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockLookup {
    Found(SymbolBlock),
    NotFound,
    /// Several top-level declarations have this name and nothing tells them apart.
    Ambiguous,
}

/// Resolves `fqn` (`name`, or `Class.member`) in `rope`. When several declarations share the
/// outermost indentation, the one starting at `hint` wins; without a match the lookup is
/// ambiguous rather than guessed.
pub fn resolve_symbol(rope: &Rope, tree: &SyntaxTree, fqn: &str, hint: Option<u32>) -> BlockLookup {
    let text = rope.to_string();
    let (head, member) = match fqn.split_once('.') {
        Some((head, member)) => (head, Some(member)),
        None => (fqn, None),
    };
    let candidates = declarations(&text, tree, head);
    let Some(min_indent) = candidates
        .iter()
        .map(|(start, _)| indentation(&text, *start))
        .min()
    else {
        return BlockLookup::NotFound;
    };
    let outermost: Vec<_> = candidates
        .into_iter()
        .filter(|(start, _)| indentation(&text, *start) == min_indent)
        .collect();
    let (start, kind) = match outermost.as_slice() {
        [only] => *only,
        many => {
            let by_hint = hint.and_then(|hint| {
                many.iter().find(|(start, _)| {
                    *start <= hint as usize && (hint as usize) < block_end(&text, *start)
                })
            });
            match by_hint {
                Some(found) => *found,
                None => return BlockLookup::Ambiguous,
            }
        }
    };
    let end = block_end(&text, start);
    match member {
        None => BlockLookup::Found(SymbolBlock {
            span_utf8: span(start, end),
            kind,
        }),
        Some(member) => match member_start(&text, start, end, member) {
            MemberLookup::Found(member_start) => BlockLookup::Found(SymbolBlock {
                span_utf8: span(member_start, block_end(&text, member_start)),
                kind: None,
            }),
            MemberLookup::NotFound => BlockLookup::NotFound,
            MemberLookup::Ambiguous => BlockLookup::Ambiguous,
        },
    }
}

/// Line starts declaring `name`: the syntax tree's, or, when the tree has none (it classifies
/// one line at a time and takes `function f(` without its `)` for a syntax error), the lines
/// whose text declares it.
fn declarations(text: &str, tree: &SyntaxTree, name: &str) -> Vec<(usize, Option<SymbolKind>)> {
    let from_tree: Vec<_> = tree
        .symbols()
        .into_iter()
        .filter(|symbol| symbol.name == name)
        .map(|symbol| (symbol.span_utf8.start.as_usize(), Some(symbol.kind)))
        .collect();
    if !from_tree.is_empty() {
        return from_tree;
    }
    let mut found = Vec::new();
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        if let Some(kind) = declared_kind(line, name) {
            found.push((line_start, Some(kind)));
        }
        line_start += line.len();
    }
    found
}

fn declared_kind(line: &str, name: &str) -> Option<SymbolKind> {
    let mut rest = line.trim_start();
    for prefix in ["export ", "default ", "declare ", "async "] {
        rest = rest.strip_prefix(prefix).map_or(rest, str::trim_start);
    }
    let (kind, after) = [
        ("function* ", SymbolKind::Function),
        ("function ", SymbolKind::Function),
        ("abstract class ", SymbolKind::Class),
        ("class ", SymbolKind::Class),
        ("interface ", SymbolKind::Interface),
        ("type ", SymbolKind::TypeAlias),
        ("const ", SymbolKind::ConstOrVariable),
        ("let ", SymbolKind::ConstOrVariable),
        ("var ", SymbolKind::ConstOrVariable),
    ]
    .into_iter()
    .find_map(|(keyword, kind)| rest.strip_prefix(keyword).map(|after| (kind, after)))?;
    let after = after.trim_start().strip_prefix(name)?;
    let boundary = after.chars().next().map_or(true, |c| {
        !(c.is_ascii_alphanumeric() || c == '_' || c == '$')
    });
    boundary.then_some(kind)
}

/// The top-level symbol whose block contains `[start, end)` of `text`, with its block.
pub fn enclosing_symbol(
    text: &str,
    tree: &SyntaxTree,
    start: usize,
    end: usize,
) -> Option<(String, Utf8Span)> {
    tree.symbols()
        .into_iter()
        .filter(|symbol| indentation(text, symbol.span_utf8.start.as_usize()) == 0)
        .find_map(|symbol| {
            let block_start = symbol.span_utf8.start.as_usize();
            let block_end = block_end(text, block_start);
            (block_start <= start && end <= block_end)
                .then(|| (symbol.name, span(block_start, block_end)))
        })
}

fn span(start: usize, end: usize) -> Utf8Span {
    Utf8Span::new(
        Utf8ByteOffset::new(start as u32),
        Utf8ByteOffset::new(end as u32),
    )
}

fn indentation(text: &str, line_start: usize) -> usize {
    text[line_start..]
        .bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Code,
    LineComment,
    BlockComment,
    Quote(u8),
    Template,
}

/// Lexical state of a scan: the open brackets (`b'T'` marks a template `${`) and the mode.
struct Scanner {
    mode: Mode,
    stack: Vec<u8>,
    opened_brace: bool,
}

impl Scanner {
    fn new() -> Self {
        Self {
            mode: Mode::Code,
            stack: Vec::new(),
            opened_brace: false,
        }
    }

    /// Feeds the byte at `i` (with `next` the byte after it); returns how many bytes it took.
    fn step(&mut self, byte: u8, next: Option<u8>) -> usize {
        match self.mode {
            Mode::LineComment => {
                if byte == b'\n' {
                    self.mode = Mode::Code;
                }
                1
            }
            Mode::BlockComment => {
                if byte == b'*' && next == Some(b'/') {
                    self.mode = Mode::Code;
                    2
                } else {
                    1
                }
            }
            Mode::Quote(quote) => {
                if byte == b'\\' {
                    2
                } else {
                    if byte == quote || byte == b'\n' {
                        self.mode = Mode::Code;
                    }
                    1
                }
            }
            Mode::Template => match byte {
                b'\\' => 2,
                b'`' => {
                    self.mode = Mode::Code;
                    1
                }
                b'$' if next == Some(b'{') => {
                    self.stack.push(b'T');
                    self.mode = Mode::Code;
                    2
                }
                _ => 1,
            },
            Mode::Code => match byte {
                b'/' if next == Some(b'/') => {
                    self.mode = Mode::LineComment;
                    2
                }
                b'/' if next == Some(b'*') => {
                    self.mode = Mode::BlockComment;
                    2
                }
                b'\'' | b'"' => {
                    self.mode = Mode::Quote(byte);
                    1
                }
                b'`' => {
                    self.mode = Mode::Template;
                    1
                }
                b'{' | b'(' | b'[' => {
                    if byte == b'{' {
                        self.opened_brace = true;
                    }
                    self.stack.push(byte);
                    1
                }
                b'}' | b')' | b']' => {
                    if self.stack.pop() == Some(b'T') {
                        self.mode = Mode::Template;
                    }
                    1
                }
                _ => 1,
            },
        }
    }

    fn balanced(&self) -> bool {
        self.stack.is_empty() && matches!(self.mode, Mode::Code | Mode::LineComment)
    }
}

/// Lines ending with one of these continue on the next line.
const CONTINUATIONS: &[&str] = &[
    "=",
    "=>",
    "(",
    "[",
    ",",
    "+",
    "-",
    "*",
    "/",
    "?",
    ":",
    "&&",
    "||",
    "??",
    ".",
    "|",
    "&",
    "<",
    "extends",
    "implements",
];

/// End (exclusive, after the newline) of the statement starting at `start`.
pub fn block_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut scanner = Scanner::new();
    let mut line_start = start;
    let mut i = start;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\n' {
            let was_comment = scanner.mode == Mode::LineComment;
            scanner.step(byte, None);
            if scanner.balanced() {
                let line = code_part(&text[line_start..i], was_comment);
                let continues = CONTINUATIONS.iter().any(|c| line.ends_with(c));
                if scanner.opened_brace || !continues {
                    return i + 1;
                }
            }
            i += 1;
            line_start = i;
            continue;
        }
        let next = bytes.get(i + 1).copied();
        i += scanner.step(byte, next).min(bytes.len() - i);
    }
    bytes.len()
}

/// A line without its trailing `//` comment (approximate: the first `//` outside quotes).
fn code_part(line: &str, had_comment: bool) -> &str {
    let line = line.trim_end_matches('\r');
    if !had_comment {
        return line.trim_end();
    }
    let mut scanner = Scanner::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if scanner.mode == Mode::Code && bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            return line[..i].trim_end();
        }
        i += scanner.step(bytes[i], bytes.get(i + 1).copied());
    }
    line.trim_end()
}

enum MemberLookup {
    Found(usize),
    NotFound,
    Ambiguous,
}

/// Start of the line declaring `member` directly in the class body `[start, end)`.
fn member_start(text: &str, start: usize, end: usize, member: &str) -> MemberLookup {
    let bytes = text.as_bytes();
    let mut scanner = Scanner::new();
    let mut found = Vec::new();
    let mut i = start;
    while i < end {
        if bytes[i] == b'\n' {
            scanner.step(b'\n', None);
            i += 1;
            if scanner.stack.len() == 1
                && scanner.mode == Mode::Code
                && declares(&text[i..end], member)
            {
                found.push(i);
            }
            continue;
        }
        i += scanner.step(bytes[i], bytes.get(i + 1).copied());
    }
    match found.as_slice() {
        [] => MemberLookup::NotFound,
        [only] => MemberLookup::Found(*only),
        _ => MemberLookup::Ambiguous,
    }
}

const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "static",
    "async",
    "readonly",
    "override",
    "abstract",
    "declare",
    "get",
    "set",
];

fn declares(line: &str, member: &str) -> bool {
    let mut rest = line.lines().next().unwrap_or("").trim_start();
    loop {
        let before = rest;
        for modifier in MODIFIERS {
            if let Some(after) = rest.strip_prefix(modifier) {
                if after.starts_with(char::is_whitespace) {
                    rest = after.trim_start();
                }
            }
        }
        rest = rest.strip_prefix('*').unwrap_or(rest).trim_start();
        if rest == before {
            break;
        }
    }
    rest.strip_prefix(member).is_some_and(|after| {
        after
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '(' | '<' | '=' | ':' | ' ' | '?' | '!'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_engine::{IncrementalAstParser, RopeTextProvider, SupportedAstLanguage};

    fn tree(rope: &Rope) -> SyntaxTree {
        IncrementalAstParser::new().parse_with(
            SupportedAstLanguage::TypeScript,
            &RopeTextProvider::new(rope),
            None,
        )
    }

    fn block_text(source: &str, fqn: &str) -> String {
        let rope = Rope::from_str(source);
        match resolve_symbol(&rope, &tree(&rope), fqn, None) {
            BlockLookup::Found(block) => {
                source[block.span_utf8.start.as_usize()..block.span_utf8.end.as_usize()].to_string()
            }
            other => panic!("{fqn}: {other:?}"),
        }
    }

    #[test]
    fn a_function_block_ends_at_its_closing_brace_despite_braces_in_strings_and_comments() {
        let source = "import x from \"y\";\n\nexport function add(a: number, b: number): number {\n  const s = \"}\"; // }\n  /* } */\n  return a + b + `${\"}\"}`.length;\n}\n\nexport const after = 1;\n";
        assert_eq!(
            block_text(source, "add"),
            "export function add(a: number, b: number): number {\n  const s = \"}\"; // }\n  /* } */\n  return a + b + `${\"}\"}`.length;\n}\n"
        );
        assert_eq!(block_text(source, "after"), "export const after = 1;\n");
    }

    #[test]
    fn multi_line_signatures_and_continued_expressions_stay_in_one_block() {
        let source = "export function f(\n  a: number,\n): number {\n  return a;\n}\nexport const g = (a: number) =>\n  a * 2;\nconst h = 3;\n";
        assert_eq!(
            block_text(source, "f"),
            "export function f(\n  a: number,\n): number {\n  return a;\n}\n"
        );
        assert_eq!(
            block_text(source, "g"),
            "export const g = (a: number) =>\n  a * 2;\n"
        );
    }

    #[test]
    fn class_members_are_found_inside_their_class_only() {
        let source = "export class Money {\n  constructor(readonly cents: number) {}\n  add(other: Money): Money {\n    return new Money(this.cents + other.cents);\n  }\n  static zero(): Money {\n    return new Money(0);\n  }\n}\nfunction add() {}\n";
        assert_eq!(
            block_text(source, "Money.add"),
            "  add(other: Money): Money {\n    return new Money(this.cents + other.cents);\n  }\n"
        );
        assert_eq!(
            block_text(source, "Money.zero"),
            "  static zero(): Money {\n    return new Money(0);\n  }\n"
        );
        let rope = Rope::from_str(source);
        assert_eq!(
            resolve_symbol(&rope, &tree(&rope), "Money.missing", None),
            BlockLookup::NotFound
        );
    }

    #[test]
    fn duplicate_names_are_ambiguous_unless_the_hint_points_at_one() {
        let source = "function dup() {\n  return 1;\n}\nfunction dup() {\n  return 2;\n}\n";
        let rope = Rope::from_str(source);
        let tree = tree(&rope);
        assert_eq!(
            resolve_symbol(&rope, &tree, "dup", None),
            BlockLookup::Ambiguous
        );
        let second = source.rfind("function dup").unwrap() as u32;
        let BlockLookup::Found(block) = resolve_symbol(&rope, &tree, "dup", Some(second)) else {
            panic!()
        };
        assert_eq!(block.span_utf8.start.as_u32(), second);
    }
}
