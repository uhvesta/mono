//! `checkleft install` — manage a git `pre-push` hook that runs checkleft
//! against the outgoing changes before a push proceeds.
//!
//! This is the trivially-easy, general-purpose path for plain git users:
//! `checkleft install` drops a `pre-push` hook into the repository's hooks
//! directory; `checkleft uninstall` (or `checkleft install --remove`)
//! reverses it. Both are idempotent and refuse to clobber a pre-existing
//! hook that checkleft did not install.
//!
//! Note: jujutsu (`jj`) implements `jj git push` natively and does **not**
//! run git hooks, so this hook does not fire for jj-driven pushes. jj-based
//! repositories (e.g. the Boss worker fleet) are covered by the Boss
//! runtime push-guard and the `cube` verb gates instead; see the README.

use std::io;
use std::path::{Path, PathBuf};

/// Marker line embedded in every checkleft-installed hook. Lets install /
/// uninstall recognise a hook they own (and leave a user's own hook
/// untouched). Stable across versions — do not change it.
pub const MANAGED_MARKER: &str = "checkleft-managed-hook";

/// Outcome of [`install_pre_push_hook`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// No prior hook existed; the checkleft hook was written.
    Installed,
    /// A checkleft-managed hook existed but differed; it was refreshed.
    Refreshed,
    /// A checkleft-managed hook already matched the desired content.
    AlreadyCurrent,
    /// A pre-push hook exists that checkleft did not install; left intact.
    RefusedForeign,
}

/// Outcome of [`uninstall_pre_push_hook`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// A checkleft-managed hook was removed.
    Removed,
    /// No pre-push hook was present.
    NotInstalled,
    /// A pre-push hook exists that checkleft did not install; left intact.
    RefusedForeign,
}

/// Path to the `pre-push` hook inside a hooks directory.
pub fn pre_push_path(hooks_dir: &Path) -> PathBuf {
    hooks_dir.join("pre-push")
}

/// Whether `content` is a hook checkleft installed (carries the marker).
pub fn is_managed(content: &str) -> bool {
    content.contains(MANAGED_MARKER)
}

/// Render the `pre-push` hook body that runs `checkleft run` via
/// `checkleft_bin`. The command is single-quote-escaped so paths with
/// spaces are handled correctly.
pub fn render_pre_push_hook(checkleft_bin: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # {MANAGED_MARKER}\n\
         #\n\
         # Installed by `checkleft install`. Runs repository convention checks\n\
         # against the outgoing changes before the push proceeds. Remove with\n\
         # `checkleft uninstall`.\n\
         exec {bin} run\n",
        bin = shell_single_quote(checkleft_bin),
    )
}

/// Install (or refresh) the checkleft `pre-push` hook in `hooks_dir`.
///
/// Idempotent: re-running with the same `checkleft_bin` is a no-op
/// ([`InstallOutcome::AlreadyCurrent`]); a checkleft-managed hook with
/// different content is refreshed; a foreign pre-push hook is left intact
/// ([`InstallOutcome::RefusedForeign`]) so a user's own hook is never
/// clobbered.
pub fn install_pre_push_hook(hooks_dir: &Path, checkleft_bin: &str) -> io::Result<InstallOutcome> {
    let path = pre_push_path(hooks_dir);
    let desired = render_pre_push_hook(checkleft_bin);

    if path.exists() {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if !is_managed(&existing) {
            return Ok(InstallOutcome::RefusedForeign);
        }
        if existing == desired {
            return Ok(InstallOutcome::AlreadyCurrent);
        }
        write_executable(&path, &desired)?;
        return Ok(InstallOutcome::Refreshed);
    }

    write_executable(&path, &desired)?;
    Ok(InstallOutcome::Installed)
}

/// Remove the checkleft `pre-push` hook from `hooks_dir`.
///
/// Idempotent: absent → [`UninstallOutcome::NotInstalled`]; a foreign hook
/// is left intact ([`UninstallOutcome::RefusedForeign`]).
pub fn uninstall_pre_push_hook(hooks_dir: &Path) -> io::Result<UninstallOutcome> {
    let path = pre_push_path(hooks_dir);
    if !path.exists() {
        return Ok(UninstallOutcome::NotInstalled);
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if !is_managed(&existing) {
        return Ok(UninstallOutcome::RefusedForeign);
    }
    std::fs::remove_file(&path)?;
    Ok(UninstallOutcome::Removed)
}

/// Write `content` to `path` and mark it executable (unix). On non-unix
/// platforms the file is written without an explicit mode change.
fn write_executable(path: &Path, content: &str) -> io::Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Single-quote a string for POSIX `sh`, escaping embedded single quotes.
fn shell_single_quote(value: &str) -> String {
    let escaped = value.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rendered_hook_carries_marker_and_runs_checkleft() {
        let hook = render_pre_push_hook("/repo/bin/checkleft");
        assert!(hook.starts_with("#!/bin/sh"), "hook must be a sh script: {hook}");
        assert!(
            hook.contains(MANAGED_MARKER),
            "hook must carry the managed marker: {hook}"
        );
        assert!(
            hook.contains("exec '/repo/bin/checkleft' run"),
            "hook must exec checkleft run: {hook}",
        );
    }

    #[test]
    fn rendered_hook_quotes_paths_with_spaces() {
        let hook = render_pre_push_hook("/Applications/My Tools/checkleft");
        assert!(
            hook.contains("exec '/Applications/My Tools/checkleft' run"),
            "path with spaces must be single-quoted: {hook}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_writes_executable_hook_when_absent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let outcome = install_pre_push_hook(dir.path(), "/bin/checkleft").unwrap();
        assert_eq!(outcome, InstallOutcome::Installed);

        let path = pre_push_path(dir.path());
        assert!(path.exists(), "hook file must exist");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "hook must be executable, mode={mode:o}");
        assert!(is_managed(&std::fs::read_to_string(&path).unwrap()));
    }

    #[test]
    fn install_is_idempotent() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            install_pre_push_hook(dir.path(), "/bin/checkleft").unwrap(),
            InstallOutcome::Installed,
        );
        // Re-running with the same binary is a no-op.
        assert_eq!(
            install_pre_push_hook(dir.path(), "/bin/checkleft").unwrap(),
            InstallOutcome::AlreadyCurrent,
        );
    }

    #[test]
    fn install_refreshes_when_binary_path_changes() {
        let dir = TempDir::new().unwrap();
        install_pre_push_hook(dir.path(), "/old/checkleft").unwrap();
        let outcome = install_pre_push_hook(dir.path(), "/new/checkleft").unwrap();
        assert_eq!(outcome, InstallOutcome::Refreshed);
        let body = std::fs::read_to_string(pre_push_path(dir.path())).unwrap();
        assert!(
            body.contains("'/new/checkleft'"),
            "hook must point at the new binary: {body}"
        );
    }

    #[test]
    fn install_refuses_to_clobber_a_foreign_hook() {
        let dir = TempDir::new().unwrap();
        let path = pre_push_path(dir.path());
        std::fs::write(&path, "#!/bin/sh\n# my own hook\necho hi\n").unwrap();
        let outcome = install_pre_push_hook(dir.path(), "/bin/checkleft").unwrap();
        assert_eq!(outcome, InstallOutcome::RefusedForeign);
        // The user's hook must be left byte-for-byte intact.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\n# my own hook\necho hi\n",
        );
    }

    #[test]
    fn uninstall_removes_a_managed_hook() {
        let dir = TempDir::new().unwrap();
        install_pre_push_hook(dir.path(), "/bin/checkleft").unwrap();
        let outcome = uninstall_pre_push_hook(dir.path()).unwrap();
        assert_eq!(outcome, UninstallOutcome::Removed);
        assert!(!pre_push_path(dir.path()).exists(), "hook must be removed");
    }

    #[test]
    fn uninstall_is_a_noop_when_absent() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            uninstall_pre_push_hook(dir.path()).unwrap(),
            UninstallOutcome::NotInstalled,
        );
    }

    #[test]
    fn uninstall_leaves_a_foreign_hook_intact() {
        let dir = TempDir::new().unwrap();
        let path = pre_push_path(dir.path());
        std::fs::write(&path, "#!/bin/sh\n# not ours\n").unwrap();
        let outcome = uninstall_pre_push_hook(dir.path()).unwrap();
        assert_eq!(outcome, UninstallOutcome::RefusedForeign);
        assert!(path.exists(), "foreign hook must be left intact");
    }
}
