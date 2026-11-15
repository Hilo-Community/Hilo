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
    // Forbidden roots are matched ignoring case: macOS and Windows file systems are
    // case-insensitive by default, so `src/SECRETS` is the folder `src/secrets`. Allowed roots
    // stay exact, which can only deny more.
    let folded = path.to_lowercase();
    allowed.iter().any(|root| under(&path, root))
        && !forbidden
            .iter()
            .any(|root| under(&folded, &root.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::permitted;

    fn check(path: &str) -> bool {
        permitted(
            path,
            &["src".into()],
            &["src/secrets".into(), ".env".into()],
        )
    }

    #[test]
    fn forbidden_roots_cannot_be_reached_by_changing_case() {
        assert!(check("src/main.rs"));
        for path in [
            "src/secrets/key",
            "src/SECRETS/key",
            "src/Secrets",
            "src/sEcReTs/a/b",
        ] {
            assert!(!check(path), "{path}");
        }
    }

    #[test]
    fn allowed_roots_stay_exact_and_traversal_is_refused() {
        assert!(!check("SRC/main.rs"));
        assert!(!check("src/../.env"));
        assert!(!check("/etc/passwd"));
        assert!(!check(r"src\secrets\key"));
    }
}
