use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use fs4::fs_std::FileExt;
use sha2::{Digest, Sha256};

use crate::app::RepobinError;

const REFRESH_TTL_DEFAULT: Duration = Duration::from_secs(300);
const TTL_ENV: &str = "REPOBIN_DEFAULTS_TTL_SECS";
const CACHE_DIR_ENV: &str = "REPOBIN_CACHE_DIR";

pub fn cache_root_from_env() -> Result<PathBuf, RepobinError> {
    if let Some(value) = env::var_os(CACHE_DIR_ENV) {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(value).join("repobin"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".cache/repobin"));
    }
    Err(RepobinError::MissingHomeDirectory)
}

#[derive(Debug, Clone)]
pub struct RepoCache {
    pub url: String,
    pub dir: PathBuf,
    pub checkout: PathBuf,
}

impl RepoCache {
    pub fn for_url(root: &Path, url: &str) -> Self {
        let dir = root.join("repos").join(repo_dir_name(url));
        let checkout = dir.join("checkout");
        Self {
            url: url.to_string(),
            dir,
            checkout,
        }
    }

    pub fn pinned_checkout_dir(&self) -> PathBuf {
        self.dir.join("pinned").join("checkout")
    }

    pub fn lock(self) -> Result<RepoCacheLock, RepobinError> {
        fs::create_dir_all(&self.dir).map_err(|source| RepobinError::CreateCacheDir {
            path: self.dir.clone(),
            source,
        })?;
        let lock_path = self.dir.join("lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| RepobinError::OpenCacheLock {
                path: lock_path.clone(),
                source,
            })?;
        FileExt::lock_exclusive(&lock_file).map_err(|source| RepobinError::AcquireCacheLock {
            path: lock_path,
            source,
        })?;
        Ok(RepoCacheLock {
            cache: self,
            _lock: lock_file,
        })
    }
}

#[derive(Debug)]
pub struct RepoCacheLock {
    cache: RepoCache,
    _lock: File,
}

#[derive(Debug, Clone)]
pub enum EnsureOutcome {
    Cloned { head: String },
    Updated { head: String },
    Cached { head: String, refreshed: bool },
    Pinned { head: String },
}

impl EnsureOutcome {
    pub fn head(&self) -> &str {
        match self {
            Self::Cloned { head }
            | Self::Updated { head }
            | Self::Cached { head, .. }
            | Self::Pinned { head } => head,
        }
    }

    pub fn note(&self) -> &'static str {
        match self {
            Self::Cloned { .. } => "cloned",
            Self::Updated { .. } => "updated",
            Self::Cached { refreshed: true, .. } => "up to date",
            Self::Cached {
                refreshed: false, ..
            } => "cached",
            Self::Pinned { .. } => "pinned",
        }
    }
}

impl RepoCacheLock {
    pub fn cache(&self) -> &RepoCache {
        &self.cache
    }

    pub fn ensure_up_to_date(&self) -> Result<EnsureOutcome, RepobinError> {
        if !self.cache.checkout.join(".git").is_dir() {
            self.clone_initial()?;
            self.update_fetch_stamp()?;
            return Ok(EnsureOutcome::Cloned {
                head: read_head(&self.cache.checkout)?,
            });
        }

        if self.fetch_within_ttl()? {
            return Ok(EnsureOutcome::Cached {
                head: read_head(&self.cache.checkout)?,
                refreshed: false,
            });
        }

        let local = read_head(&self.cache.checkout)?;
        let remote = ls_remote_head(&self.cache.checkout)?;
        if local == remote {
            self.update_fetch_stamp()?;
            return Ok(EnsureOutcome::Cached {
                head: local,
                refreshed: true,
            });
        }
        fetch_and_reset(&self.cache.checkout)?;
        let head = read_head(&self.cache.checkout)?;
        self.update_fetch_stamp()?;
        Ok(EnsureOutcome::Updated { head })
    }

    pub fn ensure_at_sha(
        &self,
        sha: &str,
        tool_name: &str,
        defaults_path: &Path,
    ) -> Result<EnsureOutcome, RepobinError> {
        let pinned_dir = self.cache.dir.join("pinned");
        let pinned_checkout = pinned_dir.join("checkout");

        fs::create_dir_all(&pinned_dir).map_err(|source| RepobinError::CreateCacheDir {
            path: pinned_dir.clone(),
            source,
        })?;

        // Reuse existing pinned checkout if it is already at the requested SHA.
        if pinned_checkout.join(".git").is_dir() {
            if let Ok(current) = read_head(&pinned_checkout) {
                if sha_matches(&current, sha) {
                    return Ok(EnsureOutcome::Pinned { head: current });
                }
            }
            // Wrong SHA or unreadable — remove and reclone.
            fs::remove_dir_all(&pinned_checkout).map_err(|source| {
                RepobinError::WriteCacheMetadata {
                    path: pinned_checkout.clone(),
                    source,
                }
            })?;
        }

        // Full clone so any reachable SHA is available.
        self.clone_full(&pinned_checkout)?;

        if !sha_reachable(&pinned_checkout, sha) {
            let _ = fs::remove_dir_all(&pinned_checkout);
            return Err(RepobinError::PinnedShaUnreachable {
                tool: tool_name.to_string(),
                sha: sha.to_string(),
                defaults_path: defaults_path.to_path_buf(),
            });
        }

        checkout_sha(&pinned_checkout, sha)?;
        let head = read_head(&pinned_checkout)?;
        Ok(EnsureOutcome::Pinned { head })
    }

    fn clone_full(&self, checkout: &Path) -> Result<(), RepobinError> {
        let output = Command::new("git")
            .arg("clone")
            .arg(&self.cache.url)
            .arg(checkout)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|source| RepobinError::SpawnGit {
                action: "clone".to_string(),
                source,
            })?;
        forward_to_stderr(&output.stdout);
        forward_to_stderr(&output.stderr);
        if !output.status.success() {
            return Err(RepobinError::GitFailed {
                action: format!("clone {}", self.cache.url),
                status: output.status.code(),
            });
        }
        Ok(())
    }

    fn clone_initial(&self) -> Result<(), RepobinError> {
        if self.cache.checkout.exists() {
            fs::remove_dir_all(&self.cache.checkout).map_err(|source| {
                RepobinError::WriteCacheMetadata {
                    path: self.cache.checkout.clone(),
                    source,
                }
            })?;
        }
        let output = Command::new("git")
            .arg("clone")
            .arg("--depth=1")
            .arg("--single-branch")
            .arg(&self.cache.url)
            .arg(&self.cache.checkout)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|source| RepobinError::SpawnGit {
                action: "clone".to_string(),
                source,
            })?;
        forward_to_stderr(&output.stdout);
        forward_to_stderr(&output.stderr);
        if !output.status.success() {
            return Err(RepobinError::GitFailed {
                action: format!("clone {}", self.cache.url),
                status: output.status.code(),
            });
        }
        let url_path = self.cache.dir.join("url");
        fs::write(&url_path, &self.cache.url).map_err(|source| {
            RepobinError::WriteCacheMetadata {
                path: url_path,
                source,
            }
        })?;
        Ok(())
    }

    fn fetch_within_ttl(&self) -> Result<bool, RepobinError> {
        let ttl = parse_ttl();
        if ttl.is_zero() {
            return Ok(false);
        }
        let stamp = self.cache.dir.join("fetch_stamp");
        let metadata = match stamp.metadata() {
            Ok(metadata) => metadata,
            Err(_) => return Ok(false),
        };
        let Ok(modified) = metadata.modified() else {
            return Ok(false);
        };
        let elapsed = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        Ok(elapsed < ttl)
    }

    fn update_fetch_stamp(&self) -> Result<(), RepobinError> {
        let stamp = self.cache.dir.join("fetch_stamp");
        File::create(&stamp).map_err(|source| RepobinError::WriteCacheMetadata {
            path: stamp,
            source,
        })?;
        Ok(())
    }
}

fn read_head(checkout: &Path) -> Result<String, RepobinError> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(checkout)
        .output()
        .map_err(|source| RepobinError::SpawnGit {
            action: "rev-parse".to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(RepobinError::GitFailed {
            action: "rev-parse HEAD".to_string(),
            status: output.status.code(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ls_remote_head(checkout: &Path) -> Result<String, RepobinError> {
    let output = Command::new("git")
        .arg("ls-remote")
        .arg("origin")
        .arg("HEAD")
        .current_dir(checkout)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| RepobinError::SpawnGit {
            action: "ls-remote".to_string(),
            source,
        })?;
    forward_to_stderr(&output.stderr);
    if !output.status.success() {
        return Err(RepobinError::GitFailed {
            action: "ls-remote origin HEAD".to_string(),
            status: output.status.code(),
        });
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let sha = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_string)
        .ok_or_else(|| RepobinError::GitFailed {
            action: "ls-remote origin HEAD".to_string(),
            status: None,
        })?;
    Ok(sha)
}

fn fetch_and_reset(checkout: &Path) -> Result<(), RepobinError> {
    let fetch = Command::new("git")
        .arg("fetch")
        .arg("--depth=1")
        .arg("origin")
        .arg("HEAD")
        .current_dir(checkout)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| RepobinError::SpawnGit {
            action: "fetch".to_string(),
            source,
        })?;
    forward_to_stderr(&fetch.stdout);
    forward_to_stderr(&fetch.stderr);
    if !fetch.status.success() {
        return Err(RepobinError::GitFailed {
            action: "fetch origin HEAD".to_string(),
            status: fetch.status.code(),
        });
    }
    let reset = Command::new("git")
        .arg("reset")
        .arg("--hard")
        .arg("FETCH_HEAD")
        .current_dir(checkout)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| RepobinError::SpawnGit {
            action: "reset".to_string(),
            source,
        })?;
    forward_to_stderr(&reset.stdout);
    forward_to_stderr(&reset.stderr);
    if !reset.status.success() {
        return Err(RepobinError::GitFailed {
            action: "reset --hard FETCH_HEAD".to_string(),
            status: reset.status.code(),
        });
    }
    Ok(())
}

fn forward_to_stderr(buf: &[u8]) {
    if buf.is_empty() {
        return;
    }
    let _ = io::stderr().write_all(buf);
}

fn sha_matches(full_sha: &str, pin: &str) -> bool {
    let (longer, shorter) = if full_sha.len() >= pin.len() {
        (full_sha, pin)
    } else {
        (pin, full_sha)
    };
    longer.starts_with(shorter)
}

fn sha_reachable(checkout: &Path, sha: &str) -> bool {
    Command::new("git")
        .args(["cat-file", "-e"])
        .arg(format!("{sha}^{{commit}}"))
        .current_dir(checkout)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn checkout_sha(checkout: &Path, sha: &str) -> Result<(), RepobinError> {
    let output = Command::new("git")
        .args(["checkout", "--detach", sha])
        .current_dir(checkout)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| RepobinError::SpawnGit {
            action: "checkout".to_string(),
            source,
        })?;
    forward_to_stderr(&output.stdout);
    forward_to_stderr(&output.stderr);
    if !output.status.success() {
        return Err(RepobinError::GitFailed {
            action: format!("checkout --detach {sha}"),
            status: output.status.code(),
        });
    }
    Ok(())
}

fn repo_dir_name(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let slug = url_slug(url);
    if slug.is_empty() {
        hex
    } else {
        format!("{slug}-{hex}")
    }
}

fn url_slug(url: &str) -> String {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let last = trimmed
        .rsplit(|c| matches!(c, '/' | ':'))
        .next()
        .unwrap_or("");
    last.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(40)
        .collect()
}

fn parse_ttl() -> Duration {
    env::var(TTL_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(REFRESH_TTL_DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::{EnsureOutcome, RepoCache, repo_dir_name, url_slug};
    use tempfile::TempDir;

    #[test]
    fn slug_uses_last_path_segment_without_git_suffix() {
        assert_eq!(url_slug("https://example.com/spinyfin/mono.git"), "mono");
        assert_eq!(url_slug("git@github.com:spinyfin/mono.git"), "mono");
        assert_eq!(url_slug("https://example.com/spinyfin/mono/"), "mono");
    }

    #[test]
    fn dir_name_includes_slug_and_hash_suffix() {
        let name = repo_dir_name("https://example.com/spinyfin/mono.git");
        assert!(name.starts_with("mono-"));
        assert_eq!(name.len(), "mono-".len() + 16);
    }

    #[test]
    fn for_url_places_checkout_under_repos_dir() {
        let temp = TempDir::new().unwrap();
        let cache = RepoCache::for_url(temp.path(), "https://example.com/spinyfin/mono.git");
        assert!(cache.dir.starts_with(temp.path().join("repos")));
        assert_eq!(cache.checkout, cache.dir.join("checkout"));
    }

    #[test]
    fn lock_excludes_concurrent_lockers() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let temp = TempDir::new().unwrap();
        let cache = RepoCache::for_url(temp.path(), "https://example.com/x.git");
        let path_for_holder = cache.clone();
        let path_for_waiter = cache.clone();

        let (acquired_tx, acquired_rx) = mpsc::channel();
        let hold = Duration::from_millis(150);

        let holder = thread::spawn(move || {
            let lock = path_for_holder.lock().unwrap();
            acquired_tx.send(()).unwrap();
            thread::sleep(hold);
            drop(lock);
        });

        acquired_rx.recv().unwrap();
        let started = Instant::now();
        let _waiter = path_for_waiter.lock().unwrap();
        let elapsed = started.elapsed();

        holder.join().unwrap();
        assert!(
            elapsed >= Duration::from_millis(50),
            "second lock returned in {elapsed:?} despite holder still holding"
        );
    }

    // Regression test: git fetch/reset output must not leak to stdout.
    // The fix routes all git subprocess output (stdout and stderr) through
    // Stdio::piped() + forward_to_stderr(), so no git fd inherits the parent's
    // stdout. This test exercises the full clone → fetch → reset path using a
    // local bare repo so no network access is needed.
    #[test]
    fn ensure_up_to_date_routes_git_output_through_stderr() {
        use std::process::Command;

        let temp = TempDir::new().unwrap();
        let remote = temp.path().join("remote.git");
        let work = temp.path().join("work");

        // Create a bare remote repo and an initial commit.
        Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&remote)
            .output()
            .unwrap();
        Command::new("git")
            .args(["clone"])
            .arg(&remote)
            .arg(&work)
            .output()
            .unwrap();
        Command::new("git")
            .args(["-c", "user.email=t@t.com", "-c", "user.name=T"])
            .args(["commit", "--allow-empty", "-m", "initial"])
            .current_dir(&work)
            .output()
            .unwrap();
        Command::new("git")
            .args(["push", "origin", "HEAD:main"])
            .current_dir(&work)
            .output()
            .unwrap();

        // Clone into the cache.
        let cache_root = temp.path().join("cache");
        let url = format!("file://{}", remote.display());
        let cache = RepoCache::for_url(&cache_root, &url);
        let lock = cache.lock().unwrap();
        let outcome = lock.ensure_up_to_date().unwrap();
        assert!(
            matches!(outcome, EnsureOutcome::Cloned { .. }),
            "expected Cloned on first call, got {outcome:?}"
        );

        // Advance the remote with a new commit.
        std::fs::write(work.join("file.txt"), "world").unwrap();
        Command::new("git")
            .args(["-c", "user.email=t@t.com", "-c", "user.name=T"])
            .args(["add", "."])
            .current_dir(&work)
            .output()
            .unwrap();
        Command::new("git")
            .args(["-c", "user.email=t@t.com", "-c", "user.name=T"])
            .args(["commit", "-m", "update"])
            .current_dir(&work)
            .output()
            .unwrap();
        Command::new("git")
            .args(["push", "origin", "HEAD:main"])
            .current_dir(&work)
            .output()
            .unwrap();

        // Remove the fetch stamp so ensure_up_to_date skips the TTL cache and
        // actually calls ls-remote + fetch_and_reset.
        let stamp = lock.cache().dir.join("fetch_stamp");
        std::fs::remove_file(&stamp).ok();

        let outcome = lock.ensure_up_to_date().unwrap();
        assert!(
            matches!(outcome, EnsureOutcome::Updated { .. }),
            "expected Updated after remote advanced, got {outcome:?}"
        );
    }

    fn make_remote_with_commits(temp: &TempDir) -> (std::path::PathBuf, Vec<String>) {
        use std::process::Command;

        let remote = temp.path().join("remote.git");
        let work = temp.path().join("work");

        Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&remote)
            .output()
            .unwrap();
        Command::new("git")
            .args(["clone"])
            .arg(&remote)
            .arg(&work)
            .output()
            .unwrap();

        let mut shas = Vec::new();
        for (i, msg) in ["commit-a", "commit-b", "commit-c"].iter().enumerate() {
            std::fs::write(work.join(format!("file{i}.txt")), msg).unwrap();
            Command::new("git")
                .args(["-c", "user.email=t@t.com", "-c", "user.name=T"])
                .args(["add", "."])
                .current_dir(&work)
                .output()
                .unwrap();
            Command::new("git")
                .args(["-c", "user.email=t@t.com", "-c", "user.name=T"])
                .args(["commit", "-m", msg])
                .current_dir(&work)
                .output()
                .unwrap();
            let sha_out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&work)
                .output()
                .unwrap();
            shas.push(
                String::from_utf8_lossy(&sha_out.stdout)
                    .trim()
                    .to_string(),
            );
            Command::new("git")
                .args(["push", "origin", "HEAD:main"])
                .current_dir(&work)
                .output()
                .unwrap();
        }
        (remote, shas)
    }

    #[test]
    fn pinned_sha_checkout_is_distinct_from_head_checkout() {
        let temp = TempDir::new().unwrap();
        let (remote, shas) = make_remote_with_commits(&temp);

        let cache_root = temp.path().join("cache");
        let url = format!("file://{}", remote.display());
        let cache = RepoCache::for_url(&cache_root, &url);

        // HEAD checkout (floating).
        let lock = cache.lock().unwrap();
        let head_outcome = lock.ensure_up_to_date().unwrap();
        assert!(matches!(head_outcome, EnsureOutcome::Cloned { .. }));
        let head_checkout = lock.cache().checkout.clone();

        // Pinned checkout at sha[0].
        let pin_outcome = lock
            .ensure_at_sha(&shas[0], "mytool", std::path::Path::new("test.yaml"))
            .unwrap();
        assert!(
            matches!(pin_outcome, EnsureOutcome::Pinned { .. }),
            "expected Pinned, got {pin_outcome:?}"
        );
        let pinned_checkout = lock.cache().pinned_checkout_dir();

        // The two checkouts must be at different paths (distinct cache slots).
        assert_ne!(
            head_checkout, pinned_checkout,
            "pinned and HEAD checkouts must be separate"
        );

        // Pinned checkout must be at sha[0].
        let pinned_sha = super::read_head(&pinned_checkout).unwrap();
        assert!(
            super::sha_matches(&pinned_sha, &shas[0]),
            "pinned checkout should be at sha[0]={}, got {pinned_sha}",
            &shas[0]
        );
    }

    // Acceptance test: pinned → unpinned → pinned-different-SHA yields right binary each time.
    #[test]
    fn pinned_then_unpinned_then_pinned_different_sha() {
        let temp = TempDir::new().unwrap();
        let (remote, shas) = make_remote_with_commits(&temp);

        let cache_root = temp.path().join("cache");
        let url = format!("file://{}", remote.display());
        let cache = RepoCache::for_url(&cache_root, &url);

        let dummy_yaml = std::path::Path::new("repobin.yaml");

        // 1. Pin to sha[0].
        let lock = cache.clone().lock().unwrap();
        let outcome = lock.ensure_at_sha(&shas[0], "mytool", dummy_yaml).unwrap();
        assert!(matches!(outcome, EnsureOutcome::Pinned { .. }));
        let pinned_sha = super::read_head(&lock.cache().pinned_checkout_dir()).unwrap();
        assert!(
            super::sha_matches(&pinned_sha, &shas[0]),
            "step 1: expected sha[0]={}, got {pinned_sha}",
            shas[0]
        );
        drop(lock);

        // 2. Floating HEAD (sha[2] is HEAD after all 3 pushes).
        let lock = cache.clone().lock().unwrap();
        let stamp = lock.cache().dir.join("fetch_stamp");
        let _ = std::fs::remove_file(&stamp);
        let outcome = lock.ensure_up_to_date().unwrap();
        assert!(
            matches!(outcome, EnsureOutcome::Cloned { .. } | EnsureOutcome::Cached { .. } | EnsureOutcome::Updated { .. }),
            "unexpected outcome: {outcome:?}"
        );
        let head_sha = super::read_head(&lock.cache().checkout).unwrap();
        assert!(
            super::sha_matches(&head_sha, &shas[2]),
            "step 2: expected sha[2]={}, got {head_sha}",
            shas[2]
        );
        drop(lock);

        // 3. Pin to sha[1].
        let lock = cache.clone().lock().unwrap();
        let outcome = lock.ensure_at_sha(&shas[1], "mytool", dummy_yaml).unwrap();
        assert!(matches!(outcome, EnsureOutcome::Pinned { .. }));
        let pinned_sha = super::read_head(&lock.cache().pinned_checkout_dir()).unwrap();
        assert!(
            super::sha_matches(&pinned_sha, &shas[1]),
            "step 3: expected sha[1]={}, got {pinned_sha}",
            shas[1]
        );

        // HEAD checkout must still be at sha[2].
        let head_sha_final = super::read_head(&lock.cache().checkout).unwrap();
        assert!(
            super::sha_matches(&head_sha_final, &shas[2]),
            "head checkout should still be sha[2]={}, got {head_sha_final}",
            shas[2]
        );
    }

    #[test]
    fn invalid_sha_returns_unreachable_error() {
        let temp = TempDir::new().unwrap();
        let (remote, _) = make_remote_with_commits(&temp);

        let cache_root = temp.path().join("cache");
        let url = format!("file://{}", remote.display());
        let cache = RepoCache::for_url(&cache_root, &url);

        let lock = cache.lock().unwrap();
        let result = lock.ensure_at_sha(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "mytool",
            std::path::Path::new("repobin.yaml"),
        );
        assert!(
            matches!(result, Err(crate::app::RepobinError::PinnedShaUnreachable { .. })),
            "expected PinnedShaUnreachable, got {result:?}"
        );
    }
}
