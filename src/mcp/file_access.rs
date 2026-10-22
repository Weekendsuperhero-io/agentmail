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
//!
//! # More than one root
//!
//! A session's workspace is its project folder, but a Project may bind
//! ADDITIONAL directories — the same list the app announces to an ACP agent as
//! `additionalDirectories` and grants to its sandbox. Those are consented
//! workspace, so a save into one has to succeed: refusing it told the user
//! their own bound folder was "outside the allowed workspace root", which is
//! both wrong and unactionable (2026-09-22).
//!
//! So the policy holds a LIST. The first entry is the PRIMARY root and is
//! privileged in exactly two ways — it is what a relative path joins onto, and
//! it is where an omitted `outputDir` writes — so default behaviour is
//! unchanged and the model still cannot be steered into an additional root by
//! accident. Containment, by contrast, accepts any root in the list: an
//! ABSOLUTE path is allowed if it lies under any one of them.
//!
//! The confinement itself is unweakened. Every root is canonicalized before
//! comparison, `..` is refused lexically, and a symlink out of one root does
//! not land inside another unless the user bound that other folder too — in
//! which case it was already reachable directly.

use std::path::{Component, Path, PathBuf};

/// Confines tool-supplied paths to the session's granted roots. See the module
/// docs — in particular, why the first root is not just one of the set.
#[derive(Debug, Clone)]
pub(crate) struct FileAccessPolicy {
    /// The sandbox roots as configured (absolute-ish but not necessarily
    /// canonical or existing yet — [`Self::ensure_root`] / [`Self::roots`] do
    /// that). NEVER empty: every constructor supplies at least one.
    ///
    /// `roots[0]` is the primary: relative joins and the default output
    /// directory. The rest widen containment only.
    roots: Vec<PathBuf>,
}

impl FileAccessPolicy {
    /// Resolve the sandbox root from the environment, falling back to a
    /// per-user directory. Never fails: a missing home directory degrades to a
    /// relative `.agentmail/files` (still a confinement, just under the cwd).
    ///
    /// Standalone mode has exactly one root — there is no Project to bind
    /// additional directories to.
    pub(crate) fn from_env() -> Self {
        let root = std::env::var_os("AGENTMAIL_FILE_ROOT")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| dirs::home_dir().map(|h| h.join(".agentmail").join("files")))
            .unwrap_or_else(|| PathBuf::from(".agentmail").join("files"));
        Self { roots: vec![root] }
    }

    /// Build a policy with a primary root plus additional granted directories.
    ///
    /// `additional` is filtered to absolute paths: a relative entry would join
    /// onto the process cwd, which is not a place anyone consented to. It is
    /// also deduplicated against the primary so a Project that binds its own
    /// folder does not produce two copies.
    pub(crate) fn with_roots(
        root: impl Into<PathBuf>,
        additional: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let primary = root.into();
        let mut roots = vec![primary.clone()];
        for extra in additional {
            if extra.is_absolute() && extra != primary && !roots.contains(&extra) {
                roots.push(extra);
            }
        }
        Self { roots }
    }

    /// The PRIMARY root: created if needed, returned canonical.
    ///
    /// Only the primary is created on demand. An additional root the user
    /// bound and then deleted should fail as absent rather than be silently
    /// recreated by an email tool.
    fn ensure_root(&self) -> Result<PathBuf, String> {
        let primary = &self.roots[0];
        std::fs::create_dir_all(primary).map_err(|e| {
            format!(
                "cannot create the file sandbox root {}: {e}",
                primary.display()
            )
        })?;
        primary.canonicalize().map_err(|e| {
            format!(
                "cannot resolve the file sandbox root {}: {e}",
                primary.display()
            )
        })
    }

    /// Every root in canonical form, primary first.
    ///
    /// An additional root that cannot be canonicalized (moved, unmounted,
    /// never existed) is DROPPED rather than erroring: it is one of several
    /// grants, and a stale one must not take down access to the others. The
    /// primary is created by [`Self::ensure_root`], so it is always present.
    fn roots(&self) -> Result<Vec<PathBuf>, String> {
        let mut out = vec![self.ensure_root()?];
        for extra in &self.roots[1..] {
            // Canonicalizing can collapse two configured roots onto one (a
            // symlinked bind), so dedupe AFTER resolving rather than before.
            if let Ok(canonical) = extra.canonicalize()
                && !out.contains(&canonical)
            {
                out.push(canonical);
            }
        }
        Ok(out)
    }

    /// Whether `candidate` (already canonical) lies inside any granted root.
    fn contained(roots: &[PathBuf], candidate: &Path) -> bool {
        roots.iter().any(|root| candidate.starts_with(root))
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

    /// Names EVERY granted root, not just the primary.
    ///
    /// With one root the old message told you where you were allowed to write.
    /// With several it would have named one of them and left the model
    /// guessing whether the others existed — and the model's next move after a
    /// refusal is to invent a path.
    fn escape_error(&self, requested: &str) -> String {
        let roots = self
            .roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if self.roots.len() == 1 {
            format!("path '{requested}' is outside the allowed workspace root ({roots})")
        } else {
            format!("path '{requested}' is outside the allowed workspace roots ({roots})")
        }
    }

    /// Confine a file to READ: it must resolve to an existing file within the
    /// root. Returns the canonical path safe to open.
    pub(crate) fn confine_read(&self, requested: &str) -> Result<PathBuf, String> {
        let roots = self.roots()?;
        // Relative paths join the PRIMARY root — see the module docs.
        let candidate = Self::resolve(&roots[0], requested)?;
        // canonicalize requires existence, which also resolves symlinks — a
        // symlink inside the root pointing out lands outside and is rejected.
        let canonical = candidate
            .canonicalize()
            .map_err(|e| format!("cannot access '{requested}': {e}"))?;
        if !Self::contained(&roots, &canonical) {
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
        let roots = self.roots()?;
        // An omitted `outputDir` writes to the PRIMARY root, never to an
        // additional one — the default must stay where the user expects it.
        let target = match requested.map(str::trim).filter(|s| !s.is_empty()) {
            None => roots[0].clone(),
            Some(p) => Self::resolve(&roots[0], p)?,
        };
        // Check the nearest existing ancestor's canonical location first.
        let mut ancestor = target.as_path();
        let existing = loop {
            if ancestor.exists() {
                break ancestor.to_path_buf();
            }
            match ancestor.parent() {
                Some(parent) => ancestor = parent,
                None => break roots[0].clone(),
            }
        };
        let canonical_ancestor = existing
            .canonicalize()
            .map_err(|e| format!("cannot resolve '{}': {e}", existing.display()))?;
        if !Self::contained(&roots, &canonical_ancestor) {
            return Err(self.escape_error(requested.unwrap_or_default()));
        }
        std::fs::create_dir_all(&target)
            .map_err(|e| format!("cannot create '{}': {e}", target.display()))?;
        let canonical = target
            .canonicalize()
            .map_err(|e| format!("cannot resolve '{}': {e}", target.display()))?;
        if !Self::contained(&roots, &canonical) {
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
        let policy = FileAccessPolicy::with_roots(&root, []);

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
        let policy = FileAccessPolicy::with_roots(&root, []);

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

    /// THE BUG THIS EXISTS FOR. A Project's additional bound directory is
    /// writable, and a directory that was never bound still is not.
    ///
    /// AgentMail confines to the session's granted roots and only ever knew
    /// about the primary one, so saving an attachment into a folder the user
    /// had deliberately bound to the Project came back as "outside the allowed
    /// workspace root" — the app refusing a grant it had already made
    /// (2026-09-22).
    #[test]
    fn an_additional_root_is_writable_and_an_unbound_directory_is_not() {
        let primary = temp_root("multi-primary");
        let bound = temp_root("multi-bound");
        let unbound = temp_root("multi-unbound");
        let policy = FileAccessPolicy::with_roots(&primary, [bound.clone()]);

        // The bound folder accepts a write, addressed absolutely — which is
        // the only way to name it, since relative paths join the primary.
        let d = policy
            .confine_dir(Some(bound.to_str().unwrap()))
            .expect("a bound additional root must be writable");
        assert_eq!(d, bound);

        // And a subdirectory of it, created on demand.
        let sub = policy
            .confine_dir(Some(bound.join("evidence").to_str().unwrap()))
            .expect("a subdir of a bound root must be writable");
        assert!(sub.starts_with(&bound) && sub.is_dir());

        // Reads work the same way.
        std::fs::write(bound.join("msg.eml"), b"hi").unwrap();
        let read = policy
            .confine_read(bound.join("msg.eml").to_str().unwrap())
            .expect("a file in a bound root must be readable");
        assert!(read.starts_with(&bound));

        // A folder nobody bound is still refused — widening the grant must not
        // become "any absolute path".
        let err = policy
            .confine_dir(Some(unbound.to_str().unwrap()))
            .expect_err("an unbound directory must stay refused");
        assert!(err.contains("outside the allowed workspace roots"), "{err}");
        // The message names every root, so the model is not left guessing.
        assert!(err.contains(primary.to_str().unwrap()), "{err}");
        assert!(err.contains(bound.to_str().unwrap()), "{err}");
    }

    /// The primary root keeps its two privileges: relative paths join it, and
    /// an omitted `outputDir` lands in it.
    ///
    /// Without this an additional root could quietly become the default write
    /// target — a save the user expected in their project appearing in some
    /// other bound folder, which is worse than a refusal because nothing says
    /// so.
    #[test]
    fn the_primary_root_still_owns_relative_paths_and_the_default() {
        let primary = temp_root("multi-primary-wins");
        let bound = temp_root("multi-bound-loses");
        let policy = FileAccessPolicy::with_roots(&primary, [bound.clone()]);

        assert_eq!(
            policy.confine_dir(None).expect("default dir"),
            primary,
            "an omitted outputDir writes to the PRIMARY root"
        );
        let rel = policy.confine_dir(Some("downloads")).expect("relative dir");
        assert!(
            rel.starts_with(&primary) && !rel.starts_with(&bound),
            "a relative path joins the PRIMARY root, got {}",
            rel.display()
        );
    }

    /// A stale additional root is dropped, not fatal.
    ///
    /// Bindings outlive the folders they name — a bound directory gets moved
    /// or unmounted. That must cost access to THAT root only; taking the whole
    /// session's file access down with it would turn a missing folder into
    /// "AgentMail cannot save anything".
    #[test]
    fn a_missing_additional_root_does_not_break_the_others() {
        let primary = temp_root("multi-stale-primary");
        let bound = temp_root("multi-stale-bound");
        let gone = std::env::temp_dir().join("agentmail-sandbox-multi-stale-gone");
        let _ = std::fs::remove_dir_all(&gone);

        let policy = FileAccessPolicy::with_roots(&primary, [gone.clone(), bound.clone()]);

        assert_eq!(
            policy.confine_dir(None).expect("primary still works"),
            primary
        );
        assert_eq!(
            policy
                .confine_dir(Some(bound.to_str().unwrap()))
                .expect("a live additional root still works"),
            bound
        );
        // The absent one is simply not a root.
        assert!(policy.confine_dir(Some(gone.to_str().unwrap())).is_err());
    }

    /// Relative and duplicate entries never become roots.
    ///
    /// A relative entry would join the process cwd — a place nobody consented
    /// to — and a Project that binds its own folder must not produce a
    /// duplicate that shows up twice in the refusal message.
    #[test]
    fn additional_roots_are_filtered_to_absolute_and_deduplicated() {
        let primary = temp_root("multi-filter");
        let policy = FileAccessPolicy::with_roots(
            &primary,
            [
                PathBuf::from("relative/not/allowed"),
                primary.clone(),
                primary.clone(),
            ],
        );
        let err = policy
            .confine_dir(Some("/definitely/not/bound"))
            .unwrap_err();
        assert!(
            err.contains("outside the allowed workspace root ("),
            "exactly one root survived, so the message is singular: {err}"
        );
    }
}
