use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellFragment {
    pub shell_name: String,
    pub config_hint: Option<String>,
    pub fragment: String,
}

pub fn bin_dir_on_path(bin_dir: &Path, path_var: Option<&OsStr>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };

    let wanted = normalize_for_compare(bin_dir);
    env::split_paths(path_var).any(|entry| normalize_for_compare(&entry) == wanted)
}

pub fn path_update_fragment(bin_dir: &Path, shell_var: Option<&OsStr>, home_dir: Option<&Path>) -> ShellFragment {
    let shell_name = shell_var
        .and_then(|value| Path::new(value).file_name())
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "sh".to_string());

    match shell_name.as_str() {
        "fish" => ShellFragment {
            shell_name,
            config_hint: Some("~/.config/fish/config.fish".to_string()),
            fragment: format!("fish_add_path {}", fish_quote(bin_dir)),
        },
        "bash" => ShellFragment {
            shell_name,
            config_hint: Some("~/.bashrc".to_string()),
            fragment: format!("export PATH={}:\"$PATH\"", posix_quote(bin_dir)),
        },
        "zsh" => ShellFragment {
            shell_name,
            config_hint: Some("~/.zshrc".to_string()),
            fragment: format!("export PATH={}:\"$PATH\"", posix_quote(bin_dir)),
        },
        _ => ShellFragment {
            shell_name,
            config_hint: home_dir.map(|_| "~/.profile".to_string()),
            fragment: format!("export PATH={}:\"$PATH\"", posix_quote(bin_dir)),
        },
    }
}

fn normalize_for_compare(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn posix_quote(path: &Path) -> String {
    let raw = path.display().to_string().replace('\'', "'\\''");
    format!("'{raw}'")
}

fn fish_quote(path: &Path) -> String {
    posix_quote(path)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::{bin_dir_on_path, path_update_fragment};

    #[test]
    fn path_update_fragment_matches_shell_conventions() {
        let zsh = path_update_fragment(
            Path::new("/tmp/bin"),
            Some(OsStr::new("/bin/zsh")),
            Some(Path::new("/Users/test")),
        );
        assert_eq!(zsh.config_hint.as_deref(), Some("~/.zshrc"));
        assert_eq!(zsh.fragment, "export PATH='/tmp/bin':\"$PATH\"");

        let fish = path_update_fragment(
            Path::new("/tmp/bin"),
            Some(OsStr::new("/usr/local/bin/fish")),
            Some(Path::new("/Users/test")),
        );
        assert_eq!(fish.config_hint.as_deref(), Some("~/.config/fish/config.fish"));
        assert_eq!(fish.fragment, "fish_add_path '/tmp/bin'");
    }

    #[test]
    fn bin_dir_on_path_canonicalizes_entries() {
        let temp = TempDir::new().expect("tempdir");
        let actual = temp.path().join("bin");
        fs::create_dir_all(&actual).expect("create dir");
        let alias_root = temp.path().join("alias-root");
        std::os::unix::fs::symlink(temp.path(), &alias_root).expect("symlink temp dir");
        let aliased = alias_root.join("bin");

        let path_var = OsString::from(aliased.as_os_str());
        assert!(bin_dir_on_path(&actual, Some(path_var.as_os_str())));
    }
}
