//! Filesystem jail confining every file tool to the model workspace
//! (`model-output/`). Rejects `..` traversal and any path that resolves
//! (directly or through symlinks) outside the jail root.

use lmhub_core::{CoreError, Result};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PathJail {
    root: PathBuf,
}

impl PathJail {
    /// Create (if needed) and canonicalize the jail root.
    pub fn create(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let canonical = root.canonicalize()?;
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user-supplied path against the jail.
    ///
    /// Absolute inputs are treated as jail-relative (leading `/` stripped),
    /// `.` components are dropped, `..` is rejected outright, and the final
    /// canonical path must remain inside the root.
    pub fn resolve(&self, user_path: &str) -> Result<PathBuf> {
        let trimmed = user_path.trim();
        let raw = Path::new(trimmed);
        let mut rel = PathBuf::new();
        for component in raw.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(c) => rel.push(c),
                Component::ParentDir => {
                    return Err(CoreError::Sandbox(format!(
                        "path traversal (`..`) rejected: {:?}",
                        trimmed
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    // Absolute paths are re-based onto the jail root.
                }
            }
        }
        let candidate = if rel.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        };
        self.validate(candidate, trimmed)
    }

    fn validate(&self, candidate: PathBuf, original: &str) -> Result<PathBuf> {
        match candidate.symlink_metadata() {
            Ok(_) => {
                let canonical = candidate.canonicalize().map_err(|e| {
                    CoreError::Sandbox(format!(
                        "cannot resolve {:?} inside workspace: {e} (dangling symlink?)",
                        original
                    ))
                })?;
                if !canonical.starts_with(&self.root) {
                    return Err(self.violation(original));
                }
                Ok(canonical)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Target does not exist yet (write/create case): walk up to
                // the deepest existing ancestor, verify containment of its
                // canonical path, then append the remaining (not-yet-existing)
                // components. A dangling symlink in the existing prefix is
                // rejected outright: `exists()` would have skipped it and a
                // later write could follow it once repointed.
                let mut base = candidate.clone();
                let mut tail: Vec<OsString> = Vec::new();
                loop {
                    match base.symlink_metadata() {
                        Ok(_) => {
                            let canonical = base.canonicalize().map_err(|_| {
                                CoreError::Sandbox(format!(
                                    "cannot resolve {:?} inside workspace: dangling symlink in path",
                                    original
                                ))
                            })?;
                            if !canonical.starts_with(&self.root) {
                                return Err(self.violation(original));
                            }
                            let mut resolved = canonical;
                            for part in tail.into_iter().rev() {
                                resolved.push(part);
                            }
                            return Ok(resolved);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            match (base.parent(), base.file_name()) {
                                (Some(p), Some(name)) => {
                                    tail.push(name.to_os_string());
                                    base = p.to_path_buf();
                                }
                                _ => {
                                    return Err(CoreError::Sandbox(format!(
                                        "cannot resolve {:?} inside workspace",
                                        original
                                    )))
                                }
                            }
                        }
                        Err(e) => return Err(CoreError::Io(e)),
                    }
                }
            }
            Err(e) => Err(CoreError::Io(e)),
        }
    }

    fn violation(&self, original: &str) -> CoreError {
        CoreError::Sandbox(format!(
            "path {:?} resolves outside the workspace jail",
            original
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jail() -> PathJail {
        let dir = tempfile::tempdir().unwrap();
        // Leak-free test jail: keep TempDir alive by leaking (tests are short).
        let root = dir.path().join("ws");
        let j = PathJail::create(&root).unwrap();
        std::mem::forget(dir);
        j
    }

    #[test]
    fn resolves_relative_paths_inside() {
        let j = jail();
        let p = j.resolve("src/main.rs").unwrap();
        assert!(p.starts_with(j.root()));
    }

    #[test]
    fn rebases_absolute_paths() {
        let j = jail();
        let p = j.resolve("/etc/passwd").unwrap();
        assert!(p.starts_with(j.root()));
        assert!(p.ends_with("etc/passwd"));
    }

    #[test]
    fn rejects_dotdot_traversal() {
        let j = jail();
        assert!(j.resolve("../secret").is_err());
        assert!(j.resolve("a/../../b").is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let j = jail();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();
        std::os::unix::fs::symlink(secret, j.root().join("leak")).unwrap();
        assert!(j.resolve("leak").is_err());
        std::mem::forget(outside);
    }

    #[test]
    fn rejects_symlinked_directory_escape() {
        let j = jail();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), j.root().join("out")).unwrap();
        assert!(j.resolve("out/file-that-does-not-exist.txt").is_err());
        std::mem::forget(outside);
    }

    #[test]
    fn allows_new_files_under_existing_dirs() {
        let j = jail();
        std::fs::create_dir_all(j.root().join("deep/nested")).unwrap();
        let p = j.resolve("deep/nested/new-file.txt").unwrap();
        assert_eq!(p, j.root().join("deep/nested/new-file.txt"));
    }

    #[test]
    fn rejects_dangling_symlink_in_write_path() {
        let j = jail();
        std::os::unix::fs::symlink("nowhere-target", j.root().join("dangling")).unwrap();
        assert!(j.resolve("dangling/new-file.txt").is_err());
    }

    #[test]
    fn follows_valid_symlink_inside_jail() {
        let j = jail();
        std::fs::create_dir_all(j.root().join("real-dir")).unwrap();
        std::os::unix::fs::symlink("real-dir", j.root().join("alias")).unwrap();
        let p = j.resolve("alias/new-file.txt").unwrap();
        assert_eq!(p, j.root().join("real-dir/new-file.txt"));
    }
}
