//! Adding patterns to a worktree's top-level `.gitignore`.
//!
//! [`exact_pattern`] turns a repository-relative path into the pattern that
//! matches only that path. [`add_ignore_pattern`] appends one pattern to the
//! `.gitignore` in the worktree root, keeping the file's line endings, and
//! leaves the file unchanged when the pattern is already one of its lines.
//! The file is replaced atomically through a capability handle on the
//! worktree root and is never followed through a symbolic link.

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The ignore file this module edits, relative to the worktree root.
pub const IGNORE_FILE: &str = ".gitignore";
/// The largest `.gitignore` read before an edit is refused.
pub const MAX_IGNORE_FILE_BYTES: u64 = 1024 * 1024;
/// The longest pattern accepted, in UTF-8 bytes.
pub const MAX_PATTERN_BYTES: usize = 4096;

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgnoreOutcome {
    /// The pattern was appended.
    Added,
    /// The file already held the pattern as a line; nothing changed.
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgnoreError {
    /// Empty, blank, a comment, too long, or containing a line break or NUL.
    InvalidPattern,
    /// `.gitignore` is a symbolic link, a directory or another special file.
    UnsafeFile,
    /// `.gitignore` exceeds [`MAX_IGNORE_FILE_BYTES`].
    TooLarge,
    /// Reading or replacing the file failed.
    Io,
}

impl std::fmt::Display for IgnoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPattern => "invalid ignore pattern",
            Self::UnsafeFile => "ignore file is not a regular file",
            Self::TooLarge => "ignore file is too large",
            Self::Io => "ignore file could not be updated",
        })
    }
}

impl std::error::Error for IgnoreError {}

/// The top-level `.gitignore` pattern that matches exactly `path`.
///
/// The pattern is anchored with a leading `/`, escapes Git's wildcard
/// characters and trailing spaces, and ends in `/` for a directory. Returns
/// `None` for an empty path, a path that is not UTF-8, or one containing a
/// line break, which no `.gitignore` line can express.
#[must_use]
pub fn exact_pattern(path: &[u8], directory: bool) -> Option<String> {
    let path = std::str::from_utf8(path).ok()?.trim_matches('/');
    if path.is_empty() || path.contains(['\n', '\r', '\0']) {
        return None;
    }
    let trailing_spaces = path.len() - path.trim_end_matches(' ').len();
    let body = &path[..path.len() - trailing_spaces];
    let mut pattern = String::with_capacity(path.len() + 8);
    pattern.push('/');
    for character in body.chars() {
        if matches!(character, '\\' | '*' | '?' | '[') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    for _ in 0..trailing_spaces {
        pattern.push_str("\\ ");
    }
    if directory {
        pattern.push('/');
    }
    Some(pattern)
}

/// Whether `pattern` can be added as one `.gitignore` line.
///
/// # Errors
///
/// [`IgnoreError::InvalidPattern`] for an empty, blank or comment line, a
/// pattern over [`MAX_PATTERN_BYTES`], or one containing a line break or NUL.
pub fn validate_pattern(pattern: &str) -> Result<(), IgnoreError> {
    if pattern.trim().is_empty()
        || pattern.starts_with('#')
        || pattern.len() > MAX_PATTERN_BYTES
        || pattern.contains(['\n', '\r', '\0'])
    {
        return Err(IgnoreError::InvalidPattern);
    }
    Ok(())
}

/// `existing` with `pattern` appended as a line, or `None` when one of its
/// lines already is `pattern`.
///
/// The new line uses CRLF when the file already does, and a missing final line
/// break is added first.
#[must_use]
pub fn appended(existing: &[u8], pattern: &str) -> Option<Vec<u8>> {
    let present = existing
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .any(|line| line == pattern.as_bytes());
    if present {
        return None;
    }
    let ending: &[u8] = if existing.windows(2).any(|pair| pair == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    };
    let mut output = Vec::with_capacity(existing.len() + pattern.len() + 4);
    output.extend_from_slice(existing);
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        output.extend_from_slice(ending);
    }
    output.extend_from_slice(pattern.as_bytes());
    output.extend_from_slice(ending);
    Some(output)
}

/// Appends `pattern` to the `.gitignore` in `worktree_root`, creating it when
/// missing.
///
/// # Errors
///
/// See [`IgnoreError`]. On error the file is unchanged.
pub fn add_ignore_pattern(
    worktree_root: &Path,
    pattern: &str,
) -> Result<IgnoreOutcome, IgnoreError> {
    validate_pattern(pattern)?;
    let root = cap_std::fs::Dir::open_ambient_dir(worktree_root, cap_std::ambient_authority())
        .map_err(|_| IgnoreError::Io)?;
    let (existing, permissions) = match root.symlink_metadata(IGNORE_FILE) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(IgnoreError::UnsafeFile);
            }
            if metadata.len() > MAX_IGNORE_FILE_BYTES {
                return Err(IgnoreError::TooLarge);
            }
            (read_bounded(&root)?, Some(metadata.permissions()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
        Err(_) => return Err(IgnoreError::Io),
    };
    let Some(content) = appended(&existing, pattern) else {
        return Ok(IgnoreOutcome::AlreadyPresent);
    };
    let temporary = format!(
        "{IGNORE_FILE}.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    );
    let result = (|| {
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = root.open_with(&temporary, &options)?;
        file.write_all(&content)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        drop(file);
        root.rename(&temporary, &root, IGNORE_FILE)
    })();
    if result.is_err() {
        let _ = root.remove_file(&temporary);
        return Err(IgnoreError::Io);
    }
    Ok(IgnoreOutcome::Added)
}

fn read_bounded(root: &cap_std::fs::Dir) -> Result<Vec<u8>, IgnoreError> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = root
        .open_with(IGNORE_FILE, &options)
        .map_err(|_| IgnoreError::Io)?;
    if !file.metadata().map_err(|_| IgnoreError::Io)?.is_file() {
        return Err(IgnoreError::UnsafeFile);
    }
    let mut content = Vec::new();
    file.take(MAX_IGNORE_FILE_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|_| IgnoreError::Io)?;
    if content.len() as u64 > MAX_IGNORE_FILE_BYTES {
        return Err(IgnoreError::TooLarge);
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_patterns_anchor_and_escape() {
        assert_eq!(
            exact_pattern(b"build/out.log", false).as_deref(),
            Some("/build/out.log")
        );
        assert_eq!(exact_pattern(b"target", true).as_deref(), Some("/target/"));
        assert_eq!(
            exact_pattern(b"a*b?[c]\\d", false).as_deref(),
            Some("/a\\*b\\?\\[c]\\\\d")
        );
        assert_eq!(exact_pattern(b"#notes", false).as_deref(), Some("/#notes"));
        assert_eq!(exact_pattern(b"!keep", false).as_deref(), Some("/!keep"));
        assert_eq!(
            exact_pattern(b"space  ", false).as_deref(),
            Some("/space\\ \\ ")
        );
        assert_eq!(exact_pattern(b"", false), None);
        assert_eq!(exact_pattern(b"line\nbreak", false), None);
        assert_eq!(exact_pattern(&[0xff, b'a'], false), None);
    }

    #[test]
    fn validation_rejects_lines_git_would_not_read_as_patterns() {
        assert!(validate_pattern("/build/").is_ok());
        assert!(validate_pattern("*.log").is_ok());
        for invalid in ["", "   ", "# comment", "a\nb", "a\rb", "a\0b"] {
            assert_eq!(validate_pattern(invalid), Err(IgnoreError::InvalidPattern));
        }
        assert!(validate_pattern(&"a".repeat(MAX_PATTERN_BYTES + 1)).is_err());
    }

    #[test]
    fn appending_keeps_line_endings_and_skips_duplicates() {
        assert_eq!(appended(b"", "/a").unwrap(), b"/a\n");
        assert_eq!(appended(b"x\n", "/a").unwrap(), b"x\n/a\n");
        assert_eq!(appended(b"x", "/a").unwrap(), b"x\n/a\n");
        assert_eq!(appended(b"x\r\ny", "/a").unwrap(), b"x\r\ny\r\n/a\r\n");
        assert_eq!(appended(b"x\r\n/a\r\n", "/a"), None);
        assert_eq!(appended(b"/a", "/a"), None);
        assert!(appended(b"/ab\n", "/a").is_some());
    }
}
