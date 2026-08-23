//! Sandbox for LLM-supplied filesystem paths.
//!
//! The MCP tools run on behalf of a model that reads UNTRUSTED email, so a
//! prompt-injection payload could try to make `create_draft` attach a sensitive
//! local file (exfiltration) or make `download_attachments` write attacker
//! bytes into a sensitive directory. Every path that originates in a tool
//! argument is therefore confined to a single sandbox ROOT: reads must resolve
//! to an existing file inside the root, writes are created inside the root, and
//! `..` traversal or symlink escapes are rejected (both root and candidate are
//! canonicalized, so a symlink pointing outside the root fails the prefix
//! check).
//!
//! Standalone mode uses `AGENTMAIL_FILE_ROOT` (or `~/.agentmail/files`). The
//! embedded server instead receives the active session workspace in trusted
//! request metadata and builds a fresh policy for that request.

use std::path::{Component, Path, PathBuf};

/// Confines tool-supplied paths to a sandbox root. See the module docs.
#[derive(Debug, Clone)]
pub(crate) struct FileAccessPolicy {
    /// The sandbox root as configured (resolved to an absolute-ish path but not
    /// necessarily canonical or existing yet — [`Self::ensure_root`] does that).
    root: PathBuf,
}

impl FileAccessPolicy {
    /// Resolve the sandbox root from the environment, falling back to a
    /// per-user directory. Never fails: a missing home directory degrades to a
    /// relative `.agentmail/files` (still a confinement, just under the cwd).
    pub(crate) fn from_env() -> Self {
        let root = std::env::var_os("AGENTMAIL_FILE_ROOT")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| dirs::home_dir().map(|h| h.join(".agentmail").join("files")))
            .unwrap_or_else(|| PathBuf::from(".agentmail").join("files"));
        Self { root }
    }

    /// Build a policy with an explicit root (tests / embedders).
    pub(crate) fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Create the root if needed and return its canonical form.
    fn ensure_root(&self) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.root).map_err(|e| {
            format!(
                "cannot create the file sandbox root {}: {e}",
                self.root.display()
            )
        })?;
        self.root.canonicalize().map_err(|e| {
            format!(
                "cannot resolve the file sandbox root {}: {e}",
                self.root.display()
            )
        })
    }

    /// Resolve a requested path against the root: absolute paths are taken as
    /// given (still subject to the containment check), relative paths join the
    /// root. Rejects any `..` component up front — a cheap lexical guard before
    /// the canonical containment check catches symlink escapes.
    fn resolve(root: &Path, requested: &str) -> Result<PathBuf, String> {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err("path is empty".to_string());
        }
        let path = Path::new(requested);
        if path.components().any(|c| c == Component::ParentDir) {
            return Err(format!("path '{requested}' must not contain '..'"));
        }
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        })
    }

    fn escape_error(&self, requested: &str) -> String {
        format!(
            "path '{requested}' is outside the allowed workspace root ({})",
            self.root.display()
        )
    }

    /// Confine a file to READ: it must resolve to an existing file within the
    /// root. Returns the canonical path safe to open.
    pub(crate) fn confine_read(&self, requested: &str) -> Result<PathBuf, String> {
        let root = self.ensure_root()?;
        let candidate = Self::resolve(&root, requested)?;
        // canonicalize requires existence, which also resolves symlinks — a
        // symlink inside the root pointing out lands outside and is rejected.
        let canonical = candidate
            .canonicalize()
            .map_err(|e| format!("cannot access '{requested}': {e}"))?;
        if !canonical.starts_with(&root) {
            return Err(self.escape_error(requested));
        }
        if !canonical.is_file() {
            return Err(format!("'{requested}' is not a regular file"));
        }
        Ok(canonical)
    }

    /// Confine an output DIRECTORY to write into, creating it within the root.
    /// `None`/empty resolves to the root itself. The nearest existing ancestor
    /// is canonicalized and checked before creation so a symlinked ancestor
    /// cannot redirect the write outside the root.
    pub(crate) fn confine_dir(&self, requested: Option<&str>) -> Result<PathBuf, String> {
        let root = self.ensure_root()?;
        let target = match requested.map(str::trim).filter(|s| !s.is_empty()) {
            None => root.clone(),
            Some(p) => Self::resolve(&root, p)?,
        };
        // Check the nearest existing ancestor's canonical location first.
        let mut ancestor = target.as_path();
        let existing = loop {
            if ancestor.exists() {
                break ancestor.to_path_buf();
            }
            match ancestor.parent() {
                Some(parent) => ancestor = parent,
                None => break root.clone(),
            }
        };
        let canonical_ancestor = existing
            .canonicalize()
            .map_err(|e| format!("cannot resolve '{}': {e}", existing.display()))?;
        if !canonical_ancestor.starts_with(&root) {
            return Err(self.escape_error(requested.unwrap_or_default()));
        }
        std::fs::create_dir_all(&target)
            .map_err(|e| format!("cannot create '{}': {e}", target.display()))?;
        let canonical = target
            .canonicalize()
            .map_err(|e| format!("cannot resolve '{}': {e}", target.display()))?;
        if !canonical.starts_with(&root) {
            return Err(self.escape_error(requested.unwrap_or_default()));
        }
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentmail-sandbox-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn read_allows_files_inside_and_rejects_escapes() {
        let root = temp_root("read");
        let policy = FileAccessPolicy::with_root(&root);

        // A file inside the root, addressed relatively, is allowed.
        std::fs::write(root.join("ok.txt"), b"hi").unwrap();
        let resolved = policy.confine_read("ok.txt").expect("in-root file allowed");
        assert!(resolved.starts_with(&root));

        // An absolute path OUTSIDE the root (the exfil case) is rejected even
        // though the file exists.
        let outside = std::env::temp_dir().join("agentmail-sandbox-outside-secret.txt");
        std::fs::write(&outside, b"secret").unwrap();
        let err = policy
            .confine_read(outside.to_str().unwrap())
            .expect_err("out-of-root read must be rejected");
        assert!(err.contains("outside the allowed workspace root"), "{err}");

        // `..` traversal is rejected lexically.
        let err = policy
            .confine_read("../escape.txt")
            .expect_err(".. traversal must be rejected");
        assert!(err.contains(".."), "{err}");

        // A symlink inside the root that points outside is rejected (canonical
        // path escapes the root).
        #[cfg(unix)]
        {
            let link = root.join("sneaky");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let err = policy
                .confine_read("sneaky")
                .expect_err("symlink escape must be rejected");
            assert!(err.contains("outside the allowed workspace root"), "{err}");
        }

        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn dir_defaults_to_root_and_confines_writes() {
        let root = temp_root("dir");
        let policy = FileAccessPolicy::with_root(&root);

        // No dir → the root itself.
        let d = policy.confine_dir(None).expect("default dir is the root");
        assert_eq!(d, root);

        // A relative subdir is created inside the root.
        let sub = policy.confine_dir(Some("downloads/day1")).expect("subdir");
        assert!(sub.starts_with(&root) && sub.is_dir());

        // An absolute dir outside the root is rejected.
        let outside = std::env::temp_dir().join("agentmail-sandbox-outside-dir");
        let err = policy
            .confine_dir(Some(outside.to_str().unwrap()))
            .expect_err("out-of-root write dir must be rejected");
        assert!(err.contains("outside the allowed workspace root"), "{err}");

        // `..` is rejected.
        let err = policy
            .confine_dir(Some("../oops"))
            .expect_err(".. must be rejected");
        assert!(err.contains(".."), "{err}");
    }
}
