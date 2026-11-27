//! ACP agents write whole files (`fs/write_text_file`); the Judge reasons about symbols. A
//! proposal becomes one `SymbolSearchReplace` per changed region, anchored to the top-level
//! symbol around it and with enough surrounding lines to be unique there. Compiled against the
//! current text, each region either still finds its lines (unrelated user edits are kept) or is
//! a conflict.

use host_core::{ropey::Rope, ChangeOperation, ChangeSet, CONTRACT_SCHEMA_VERSION};

use super::blocks::enclosing_symbol;
use super::compiler::{hash, parse};

/// Lines of unchanged context kept around a change before checking it is unique.
const CONTEXT_LINES: usize = 2;
/// Above this many cells the line diff gives up on precision: one region covers the middle.
const MAX_DIFF_CELLS: usize = 4_000_000;

/// A file's proposed content and the content it was written against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProposal {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

/// The `ChangeSet` replacing each changed region of each file. Files whose text is unchanged
/// contribute nothing; `None` when nothing changes at all.
#[must_use]
pub fn changeset_from_proposals(
    changeset_id: &str,
    task_id: &str,
    base_revision: u32,
    proposals: &[FileProposal],
) -> Option<ChangeSet> {
    let mut operations = Vec::new();
    let mut affected_files = Vec::new();
    for proposal in proposals {
        let file_ops = file_operations(proposal);
        if !file_ops.is_empty() {
            affected_files.push(proposal.path.clone());
            operations.extend(file_ops);
        }
    }
    if operations.is_empty() {
        return None;
    }
    Some(ChangeSet {
        schema_version: CONTRACT_SCHEMA_VERSION,
        changeset_id: changeset_id.to_string(),
        task_id: task_id.to_string(),
        base_revision,
        operations,
        affected_files,
        result_digest: String::new(),
    })
}

/// A changed region: old lines `[old.0, old.1)` become new lines `[new.0, new.1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hunk {
    old: (usize, usize),
    new: (usize, usize),
}

fn file_operations(proposal: &FileProposal) -> Vec<ChangeOperation> {
    if proposal.old_text == proposal.new_text {
        return Vec::new();
    }
    let whole_file = || {
        vec![ChangeOperation::SymbolSearchReplace {
            file_path: proposal.path.clone(),
            anchor_symbol: String::new(),
            search_block: proposal.old_text.clone(),
            replace_block: proposal.new_text.clone(),
            expected_before_blake3: hash(&proposal.old_text),
        }]
    };
    if proposal.old_text.is_empty() {
        return whole_file();
    }
    let old: Vec<&str> = proposal.old_text.split_inclusive('\n').collect();
    let new: Vec<&str> = proposal.new_text.split_inclusive('\n').collect();
    let offsets = line_offsets(&old);
    let rope = Rope::from_str(&proposal.old_text);
    let tree = parse(&rope, &proposal.path);

    let anchors = Anchors::new(&proposal.old_text, &tree, &offsets);
    let mut regions: Vec<Region> = diff(&old, &new)
        .into_iter()
        .map(|hunk| Region {
            hunk,
            context: CONTEXT_LINES,
            bounds: anchors.bounds_of(hunk),
        })
        .collect();
    loop {
        merge_touching(&mut regions, &anchors);
        let mut grew = false;
        for region in &mut regions {
            let (start, end) = region.extended();
            let search = &proposal.old_text[offsets[start]..offsets[end]];
            let scope = anchors.scope_text(&region.bounds);
            if !search.is_empty() && scope.matches(search).count() == 1 {
                continue;
            }
            if start > region.bounds.start || end < region.bounds.end {
                region.context += 1;
                grew = true;
            } else if region.bounds.anchor.is_some() {
                // Not unique even across its whole symbol: search the file instead.
                region.bounds = anchors.whole();
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let mut operations = Vec::with_capacity(regions.len());
    for region in regions {
        let (start, end) = region.extended();
        let search = &proposal.old_text[offsets[start]..offsets[end]];
        if search.is_empty() {
            return whole_file();
        }
        let mut replace = String::new();
        for line in &old[start..region.hunk.old.0] {
            replace.push_str(line);
        }
        for line in &new[region.hunk.new.0..region.hunk.new.1] {
            replace.push_str(line);
        }
        for line in &old[region.hunk.old.1..end] {
            replace.push_str(line);
        }
        operations.push(ChangeOperation::SymbolSearchReplace {
            file_path: proposal.path.clone(),
            anchor_symbol: region.bounds.anchor.clone().unwrap_or_default(),
            search_block: search.to_string(),
            replace_block: replace,
            expected_before_blake3: hash(anchors.scope_text(&region.bounds)),
        });
    }
    operations
}

/// The lines a region's context may grow over: its symbol's block, or the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bounds {
    start: usize,
    end: usize,
    anchor: Option<String>,
}

#[derive(Debug, Clone)]
struct Region {
    hunk: Hunk,
    context: usize,
    bounds: Bounds,
}

impl Region {
    /// The old lines searched for: the hunk and its context, inside its bounds.
    fn extended(&self) -> (usize, usize) {
        (
            self.hunk
                .old
                .0
                .saturating_sub(self.context)
                .max(self.bounds.start),
            (self.hunk.old.1 + self.context).min(self.bounds.end),
        )
    }
}

/// Top-level symbols of the old text, as line ranges.
struct Anchors<'a> {
    text: &'a str,
    offsets: &'a [usize],
    tree: &'a context_engine::SyntaxTree,
}

impl<'a> Anchors<'a> {
    fn new(text: &'a str, tree: &'a context_engine::SyntaxTree, offsets: &'a [usize]) -> Self {
        Self {
            text,
            offsets,
            tree,
        }
    }

    fn whole(&self) -> Bounds {
        Bounds {
            start: 0,
            end: self.offsets.len() - 1,
            anchor: None,
        }
    }

    /// The block of the top-level symbol around the changed lines, if exactly one symbol of
    /// that name exists (the compiler refuses an ambiguous anchor).
    fn bounds_of(&self, hunk: Hunk) -> Bounds {
        let (from, to) = (self.offsets[hunk.old.0], self.offsets[hunk.old.1]);
        let Some((name, block)) = enclosing_symbol(self.text, self.tree, from, to) else {
            return self.whole();
        };
        let declared_once = self
            .tree
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == name)
            .count()
            == 1;
        let line = |byte: usize| self.offsets.binary_search(&byte).ok();
        match (
            declared_once,
            line(block.start.as_usize()),
            line(block.end.as_usize()),
        ) {
            (true, Some(start), Some(end)) => Bounds {
                start,
                end,
                anchor: Some(name),
            },
            _ => self.whole(),
        }
    }

    fn scope_text(&self, bounds: &Bounds) -> &'a str {
        &self.text[self.offsets[bounds.start]..self.offsets[bounds.end]]
    }
}

/// Merges regions whose searched lines touch; regions of different symbols merge file-wide.
fn merge_touching(regions: &mut Vec<Region>, anchors: &Anchors<'_>) {
    let mut merged: Vec<Region> = Vec::with_capacity(regions.len());
    for region in regions.drain(..) {
        if let Some(last) = merged.last_mut() {
            if region.extended().0 <= last.extended().1 {
                last.hunk.old.1 = region.hunk.old.1;
                last.hunk.new.1 = region.hunk.new.1;
                last.context = last.context.max(region.context);
                if last.bounds != region.bounds {
                    last.bounds = anchors.whole();
                }
                continue;
            }
        }
        merged.push(region);
    }
    *regions = merged;
}

fn line_offsets(lines: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len() + 1);
    let mut total = 0;
    offsets.push(0);
    for line in lines {
        total += line.len();
        offsets.push(total);
    }
    offsets
}

/// Line hunks between `old` and `new`: common prefix and suffix first, then a longest common
/// subsequence on the rest.
fn diff(old: &[&str], new: &[&str]) -> Vec<Hunk> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old_mid, new_mid) = (
        &old[prefix..old.len() - suffix],
        &new[prefix..new.len() - suffix],
    );
    if old_mid.is_empty() && new_mid.is_empty() {
        return Vec::new();
    }
    let whole = Hunk {
        old: (prefix, prefix + old_mid.len()),
        new: (prefix, prefix + new_mid.len()),
    };
    if old_mid.is_empty()
        || new_mid.is_empty()
        || (old_mid.len() + 1) * (new_mid.len() + 1) > MAX_DIFF_CELLS
    {
        return vec![whole];
    }
    let (n, m) = (old_mid.len(), new_mid.len());
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if old_mid[i] == new_mid[j] {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }
    let mut hunks = Vec::new();
    let (mut i, mut j) = (0, 0);
    let mut open: Option<(usize, usize)> = None;
    while i < n || j < m {
        if i < n && j < m && old_mid[i] == new_mid[j] {
            if let Some((oi, oj)) = open.take() {
                hunks.push(Hunk {
                    old: (prefix + oi, prefix + i),
                    new: (prefix + oj, prefix + j),
                });
            }
            i += 1;
            j += 1;
            continue;
        }
        open.get_or_insert((i, j));
        if j < m && (i == n || lcs[at(i, j + 1)] >= lcs[at(i + 1, j)]) {
            j += 1;
        } else {
            i += 1;
        }
    }
    if let Some((oi, oj)) = open {
        hunks.push(Hunk {
            old: (prefix + oi, prefix + n),
            new: (prefix + oj, prefix + m),
        });
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::compiler::{Anchoring, AstChangeSetCompiler};

    const OLD: &str = "export function add(a: number, b: number): number {\n  return a + b;\n}\n\nexport function sub(a: number, b: number): number {\n  return a + b;\n}\n";

    fn compile(cs: &ChangeSet, current: &str) -> Result<String, crate::judge::CompileError> {
        AstChangeSetCompiler::compile_changeset(cs, |_| Some(Rope::from_str(current)))
            .map(|compiled| compiled.files[0].result_text.clone())
    }

    fn proposal(new: &str) -> ChangeSet {
        changeset_from_proposals(
            "cs",
            "task",
            3,
            &[FileProposal {
                path: "src/money.ts".into(),
                old_text: OLD.into(),
                new_text: new.into(),
            }],
        )
        .unwrap()
    }

    #[test]
    fn a_proposal_round_trips_through_the_compiler_on_its_own_base() {
        let new = OLD
            .replacen(
                "  return a + b;\n}\n\nexport function sub",
                "  return a + b;\n}\n\n/** Subtracts. */\nexport function sub",
                1,
            )
            .replacen(
                "export function sub(a: number, b: number): number {\n  return a + b;",
                "export function sub(a: number, b: number): number {\n  return a - b;",
                1,
            );
        let cs = proposal(&new);
        assert_eq!(compile(&cs, OLD).unwrap(), new);
        assert_eq!(cs.base_revision, 3);
        assert!(changeset_from_proposals(
            "cs",
            "t",
            0,
            &[FileProposal {
                path: "a.ts".into(),
                old_text: OLD.into(),
                new_text: OLD.into(),
            }]
        )
        .is_none());
    }

    #[test]
    fn identical_lines_in_two_functions_are_told_apart_by_their_anchor() {
        let new = OLD.replacen(
            "export function sub(a: number, b: number): number {\n  return a + b;",
            "export function sub(a: number, b: number): number {\n  return a - b;",
            1,
        );
        let cs = proposal(&new);
        let ChangeOperation::SymbolSearchReplace { anchor_symbol, .. } = &cs.operations[0] else {
            panic!()
        };
        assert_eq!(anchor_symbol, "sub");
        assert_eq!(compile(&cs, OLD).unwrap(), new);
    }

    #[test]
    fn a_user_edit_elsewhere_is_kept_and_one_on_the_changed_lines_conflicts() {
        let new = OLD.replacen(
            "export function sub(a: number, b: number): number {\n  return a + b;",
            "export function sub(a: number, b: number): number {\n  return a - b;",
            1,
        );
        let cs = proposal(&new);
        let user_elsewhere = OLD.replacen("  return a + b;\n", "  return b + a; // user\n", 1);
        let merged = compile(&cs, &user_elsewhere).unwrap();
        assert!(merged.contains("return b + a; // user"));
        assert!(merged.contains("return a - b;"));
        let compiled =
            AstChangeSetCompiler::compile_changeset(&cs, |_| Some(Rope::from_str(&user_elsewhere)))
                .unwrap();
        assert_eq!(compiled.files[0].anchoring, Anchoring::Exact);

        let (head, tail) = OLD.rsplit_once("  return a + b;").unwrap();
        let user_on_target = format!("{head}  return a + b + 1;{tail}");
        let error = compile(&cs, &user_on_target).unwrap_err();
        assert!(error.is_conflict(), "{error}");
    }

    #[test]
    fn insertions_at_the_edges_and_new_content_in_an_empty_file_compile() {
        let appended = format!("{OLD}export const zero = 0;\n");
        assert_eq!(compile(&proposal(&appended), OLD).unwrap(), appended);
        let prepended = format!("// header\n{OLD}");
        assert_eq!(compile(&proposal(&prepended), OLD).unwrap(), prepended);
        let created = changeset_from_proposals(
            "cs",
            "t",
            0,
            &[FileProposal {
                path: "src/new.ts".into(),
                old_text: String::new(),
                new_text: "export const x = 1;\n".into(),
            }],
        )
        .unwrap();
        assert_eq!(compile(&created, "").unwrap(), "export const x = 1;\n");
        assert!(compile(&created, "user typed\n").unwrap_err().is_conflict());
    }
}
