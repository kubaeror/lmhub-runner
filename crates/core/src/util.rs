//! Small shared utilities: UTF-8-safe truncation and atomic file writes.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Truncate `s` to at most `max_chars` chars, appending a marker when cut.
///
/// Byte slicing (`&s[..n]`) panics when `n` lands inside a multi-byte
/// character — model output and tool results are full of emoji and CJK, so
/// every truncation site must go through this helper.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Write `contents` to `path` atomically: temp file in the same directory,
/// fsync, then rename. Readers never observe a half-written document.
///
/// The temp name keeps the full file name (`statistics.json.tmp`) so two
/// different documents in one directory cannot collide on `x.tmp`.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_respects_utf8_boundaries() {
        // 300 bytes ending in a 4-byte emoji: byte slicing would panic.
        let s = format!("{}🦀", "a".repeat(296));
        let out = truncate_chars(&s, 50);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 50);
        // The untouched tail must still be valid UTF-8 by construction.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());

        let short = truncate_chars("héllo", 10);
        assert_eq!(short, "héllo");
        let exact = truncate_chars("abcde", 5);
        assert_eq!(exact, "abcde");
    }

    #[test]
    fn write_atomic_replaces_content_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        write_atomic(&path, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!dir.path().join("doc.json.tmp").exists());
    }
}
