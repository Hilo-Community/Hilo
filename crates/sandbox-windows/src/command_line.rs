//! The two pieces of text `CreateProcessW` takes, built without `unsafe` (and so testable on any
//! OS): the command line and the environment block.
//!
//! The command line follows the rules every C runtime uses to split it back into arguments
//! (the same as the Rust standard library): an argument with a space, a tab or a quote is
//! wrapped in quotes, a quote inside is preceded by a backslash, and backslashes before a quote
//! are doubled. The program itself is passed separately as the application name and is never
//! searched in `PATH`.

use std::fmt;

/// Longest command line `CreateProcessW` accepts, in UTF-16 units, terminator included.
const MAX_COMMAND_LINE: usize = 32_767;

/// Why a command line or an environment block could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextError {
    /// A string contains a NUL: Windows would cut it there.
    InteriorNul,
    /// The program path contains a quote: it cannot be quoted safely.
    QuoteInProgram,
    /// An environment variable has an empty name or a name containing `=`.
    BadVariableName,
    /// Two environment variables have the same name, ignoring case.
    DuplicateVariable,
    /// The command line is longer than Windows allows.
    TooLong,
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TextError::InteriorNul => "a string contains a NUL character",
            TextError::QuoteInProgram => "the program path contains a quote",
            TextError::BadVariableName => "an environment variable has an invalid name",
            TextError::DuplicateVariable => "two environment variables differ only by case",
            TextError::TooLong => "the command line is too long",
        })
    }
}

impl std::error::Error for TextError {}

const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;
const EQUALS: u16 = b'=' as u16;

fn append_argument(out: &mut Vec<u16>, arg: &[u16]) {
    let quote = arg.is_empty() || arg.iter().any(|&unit| matches!(unit, SPACE | TAB | QUOTE));
    if quote {
        out.push(QUOTE);
    }
    let mut backslashes = 0usize;
    for &unit in arg {
        if unit == BACKSLASH {
            backslashes += 1;
        } else {
            if unit == QUOTE {
                // n backslashes before an inner quote become 2n + 1.
                out.resize(out.len() + backslashes + 1, BACKSLASH);
            }
            backslashes = 0;
        }
        out.push(unit);
    }
    if quote {
        // n backslashes before the closing quote become 2n.
        out.resize(out.len() + backslashes, BACKSLASH);
        out.push(QUOTE);
    }
}

/// `"program" arg1 arg2 ...`, without a terminating NUL.
pub fn build_command_line(program: &[u16], args: &[Vec<u16>]) -> Result<Vec<u16>, TextError> {
    if program.contains(&0) || args.iter().any(|arg| arg.contains(&0)) {
        return Err(TextError::InteriorNul);
    }
    if program.contains(&QUOTE) {
        return Err(TextError::QuoteInProgram);
    }
    let mut line = Vec::with_capacity(program.len() + 8);
    line.push(QUOTE);
    line.extend_from_slice(program);
    line.push(QUOTE);
    for arg in args {
        line.push(SPACE);
        append_argument(&mut line, arg);
    }
    if line.len() + 1 > MAX_COMMAND_LINE {
        return Err(TextError::TooLong);
    }
    Ok(line)
}

/// `NAME=value\0NAME2=value2\0\0`, sorted by name as Windows expects. Names are
/// case-insensitive on Windows, so two names that differ only by case are refused rather than
/// leaving the choice to the program. An empty environment is two NULs: `CreateProcessW` then
/// sees an empty block instead of inheriting the Host's.
pub fn build_environment_block(vars: &[(Vec<u16>, Vec<u16>)]) -> Result<Vec<u16>, TextError> {
    for (name, value) in vars {
        if name.contains(&0) || value.contains(&0) {
            return Err(TextError::InteriorNul);
        }
        if name.is_empty() || name.contains(&EQUALS) {
            return Err(TextError::BadVariableName);
        }
    }
    let mut sorted: Vec<_> = vars
        .iter()
        .map(|pair| (String::from_utf16_lossy(&pair.0).to_uppercase(), pair))
        .collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    if sorted.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(TextError::DuplicateVariable);
    }
    let mut block = Vec::new();
    for (_, (name, value)) in sorted {
        block.extend_from_slice(name);
        block.push(EQUALS);
        block.extend_from_slice(value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn line(program: &str, args: &[&str]) -> String {
        let args: Vec<Vec<u16>> = args.iter().map(|arg| w(arg)).collect();
        String::from_utf16(&build_command_line(&w(program), &args).unwrap()).unwrap()
    }

    #[test]
    fn plain_arguments_are_left_alone() {
        assert_eq!(
            line(r"C:\a\b.exe", &["x", "/c", "dir"]),
            r#""C:\a\b.exe" x /c dir"#
        );
    }

    #[test]
    fn spaces_tabs_and_empty_arguments_are_quoted() {
        assert_eq!(
            line("p.exe", &["a b", "", "t\tt"]),
            "\"p.exe\" \"a b\" \"\" \"t\tt\""
        );
    }

    #[test]
    fn quotes_are_escaped_and_backslashes_before_them_doubled() {
        assert_eq!(line("p.exe", &[r#"a"b"#]), r#""p.exe" "a\"b""#);
        assert_eq!(line("p.exe", &[r#"a\"b"#]), r#""p.exe" "a\\\"b""#);
    }

    #[test]
    fn trailing_backslashes_are_doubled_only_inside_quotes() {
        assert_eq!(line("p.exe", &[r"C:\dir\"]), r#""p.exe" C:\dir\"#);
        assert_eq!(line("p.exe", &[r"C:\my dir\"]), r#""p.exe" "C:\my dir\\""#);
    }

    #[test]
    fn a_nul_or_a_quoted_program_is_refused() {
        assert_eq!(
            build_command_line(&w("p.exe"), &[vec![b'a' as u16, 0]]),
            Err(TextError::InteriorNul)
        );
        assert_eq!(
            build_command_line(&w(r#"C:\a"b.exe"#), &[]),
            Err(TextError::QuoteInProgram)
        );
    }

    #[test]
    fn a_command_line_too_long_is_refused() {
        let huge = vec![b'a' as u16; MAX_COMMAND_LINE];
        assert_eq!(
            build_command_line(&w("p.exe"), &[huge]),
            Err(TextError::TooLong)
        );
    }

    #[test]
    fn the_environment_block_is_sorted_and_double_terminated() {
        let block = build_environment_block(&[(w("b"), w("2")), (w("A"), w("1"))]).unwrap();
        assert_eq!(String::from_utf16(&block).unwrap(), "A=1\0b=2\0\0");
    }

    #[test]
    fn an_empty_environment_is_two_nuls() {
        assert_eq!(build_environment_block(&[]).unwrap(), vec![0, 0]);
    }

    #[test]
    fn names_that_differ_only_by_case_are_refused() {
        assert_eq!(
            build_environment_block(&[(w("Path"), w("a")), (w("PATH"), w("b"))]),
            Err(TextError::DuplicateVariable)
        );
    }

    #[test]
    fn bad_variable_names_are_refused() {
        assert_eq!(
            build_environment_block(&[(w(""), w("x"))]),
            Err(TextError::BadVariableName)
        );
        assert_eq!(
            build_environment_block(&[(w("a=b"), w("x"))]),
            Err(TextError::BadVariableName)
        );
        assert_eq!(
            build_environment_block(&[(w("a"), vec![0])]),
            Err(TextError::InteriorNul)
        );
    }
}
