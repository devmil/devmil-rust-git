//! Native path comparison for registrations whose checkout may no longer exist.
use std::path::{Path, PathBuf};

fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut tail = Vec::new();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(ancestor) {
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) else {
            return path.to_path_buf();
        };
        tail.push(name);
        ancestor = parent;
    }
}

/// Compares native worktree paths through their existing ancestors, including missing roots.
pub fn same_worktree_path(left: &Path, right: &Path) -> bool {
    let left = resolve_existing_ancestor(left);
    let right = resolve_existing_ancestor(right);
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}
