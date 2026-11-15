//! Lexical worktree paths are portable and checked without consulting untrusted files.

/// Reject platform-specific roots and traversal on every OS, including Windows syntax on Unix.
pub(crate) fn normalize(path: &str) -> Option<String> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', ':', '\0']) {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            ".." => return None,
            "" | "." => {}
            value => parts.push(value),
        }
    }
    Some(parts.join("/"))
}

fn under(path: &str, root: &str) -> bool {
    root.is_empty() || path == root || path.strip_prefix(root).is_some_and(|s| s.starts_with('/'))
}

pub(crate) fn permitted(path: &str, allowed: &[String], forbidden: &[String]) -> bool {
    let Some(path) = normalize(path) else {
        return false;
    };
    let Some(allowed) = allowed
        .iter()
        .map(|p| normalize(p))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let Some(forbidden) = forbidden
        .iter()
        .map(|p| normalize(p))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    allowed.iter().any(|root| under(&path, root))
        && !forbidden.iter().any(|root| under(&path, root))
}
