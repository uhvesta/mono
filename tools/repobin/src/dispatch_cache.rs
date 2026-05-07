use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::RepobinError;

const CACHE_VERSION: u32 = 1;
const CACHE_SUBDIR: &str = "dispatch";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    repo_root: String,
    target: String,
    executable_path: String,
    binary_mtime_ns: u128,
    build_witnesses: Vec<BuildWitness>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BuildWitness {
    path: String,
    mtime_ns: u128,
}

pub fn lookup_in(cache_root: &Path, repo_root: &Path, target: &str) -> Option<PathBuf> {
    let entry_path = entry_path(cache_root, repo_root, target);
    let raw = fs::read(&entry_path).ok()?;
    let cache: CacheFile = serde_json::from_slice(&raw).ok()?;

    if cache.version != CACHE_VERSION {
        return None;
    }
    if cache.repo_root != repo_root.to_string_lossy() {
        return None;
    }
    if cache.target != target {
        return None;
    }

    let executable = PathBuf::from(&cache.executable_path);
    let actual_binary_mtime = mtime_ns(&executable)?;
    if actual_binary_mtime < cache.binary_mtime_ns {
        return None;
    }

    for witness in &cache.build_witnesses {
        let witness_path = Path::new(&witness.path);
        let actual = mtime_ns(witness_path)?;
        if actual != witness.mtime_ns {
            return None;
        }
    }

    Some(executable)
}

pub fn record_in(
    cache_root: &Path,
    repo_root: &Path,
    target: &str,
    executable_path: &Path,
) -> Result<(), RepobinError> {
    let Some(binary_mtime_ns) = mtime_ns(executable_path) else {
        return Ok(());
    };

    let build_witnesses = build_witnesses_for(repo_root, target);

    let cache = CacheFile {
        version: CACHE_VERSION,
        repo_root: repo_root.to_string_lossy().into_owned(),
        target: target.to_string(),
        executable_path: executable_path.to_string_lossy().into_owned(),
        binary_mtime_ns,
        build_witnesses,
    };

    let entry_path = entry_path(cache_root, repo_root, target);
    if let Some(parent) = entry_path.parent() {
        fs::create_dir_all(parent).map_err(|source| RepobinError::CreateCacheDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let serialized = serde_json::to_vec(&cache).map_err(io::Error::other).map_err(
        |source| RepobinError::WriteCacheMetadata {
            path: entry_path.clone(),
            source,
        },
    )?;

    write_atomic(&entry_path, &serialized)
}

fn build_witnesses_for(repo_root: &Path, target: &str) -> Vec<BuildWitness> {
    let Some(package_dir) = package_dir_for(target) else {
        return Vec::new();
    };
    let dir = repo_root.join(package_dir);
    let mut out = Vec::new();
    for candidate in ["BUILD.bazel", "BUILD"] {
        let path = dir.join(candidate);
        if let Some(mtime_ns) = mtime_ns(&path) {
            out.push(BuildWitness {
                path: path.to_string_lossy().into_owned(),
                mtime_ns,
            });
        }
    }
    out
}

fn package_dir_for(target: &str) -> Option<String> {
    let rest = target.strip_prefix("//")?;
    let pkg = match rest.split_once(':') {
        Some((pkg, _)) => pkg,
        None => rest,
    };
    Some(pkg.to_string())
}

fn entry_path(cache_root: &Path, repo_root: &Path, target: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(target.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    cache_root.join(CACHE_SUBDIR).join(format!("{hex}.json"))
}

fn mtime_ns(path: &Path) -> Option<u128> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_nanos())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), RepobinError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".dispatch-cache-")
        .suffix(".json.tmp")
        .tempfile_in(parent)
        .map_err(|source| RepobinError::WriteCacheMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    use std::io::Write;
    tmp.as_file_mut()
        .write_all(bytes)
        .map_err(|source| RepobinError::WriteCacheMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    tmp.as_file_mut()
        .sync_data()
        .map_err(|source| RepobinError::WriteCacheMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    tmp.persist(path)
        .map_err(|err| RepobinError::WriteCacheMetadata {
            path: path.to_path_buf(),
            source: err.error,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;

    use super::{
        BuildWitness, CACHE_VERSION, CacheFile, entry_path, lookup_in, package_dir_for, record_in,
    };

    fn touch(path: &std::path::Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn bump_mtime(path: &std::path::Path) {
        let later = SystemTime::now() + Duration::from_secs(2);
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(later).unwrap();
    }

    #[test]
    fn package_dir_extracts_package_path() {
        assert_eq!(
            package_dir_for("//tools/boss/cli:boss").as_deref(),
            Some("tools/boss/cli")
        );
        assert_eq!(
            package_dir_for("//tools/boss/cli").as_deref(),
            Some("tools/boss/cli")
        );
        assert_eq!(package_dir_for("//:boss").as_deref(), Some(""));
        assert_eq!(package_dir_for("not-a-label"), None);
    }

    #[test]
    fn cold_lookup_returns_none() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        let result = lookup_in(&cache_root, &repo_root, "//tools/boss/cli:boss");
        assert!(result.is_none());
    }

    #[test]
    fn warm_hit_returns_recorded_path() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");
        let build = repo_root.join("tools/boss/cli/BUILD.bazel");

        touch(&exe, "binary");
        touch(&build, "rust_binary(...)");

        record_in(&cache_root, &repo_root, target, &exe).expect("record");

        let hit = lookup_in(&cache_root, &repo_root, target).expect("hit");
        assert_eq!(hit, exe);
    }

    #[test]
    fn missing_binary_is_a_miss() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");
        let build = repo_root.join("tools/boss/cli/BUILD.bazel");

        touch(&exe, "binary");
        touch(&build, "rust_binary(...)");
        record_in(&cache_root, &repo_root, target, &exe).expect("record");

        fs::remove_file(&exe).unwrap();
        assert!(lookup_in(&cache_root, &repo_root, target).is_none());
    }

    #[test]
    fn build_mtime_advance_invalidates() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");
        let build = repo_root.join("tools/boss/cli/BUILD.bazel");

        touch(&exe, "binary");
        touch(&build, "rust_binary(...)");
        record_in(&cache_root, &repo_root, target, &exe).expect("record");

        assert!(lookup_in(&cache_root, &repo_root, target).is_some());
        bump_mtime(&build);
        assert!(lookup_in(&cache_root, &repo_root, target).is_none());
    }

    #[test]
    fn corrupt_cache_falls_through() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        fs::create_dir_all(&repo_root).unwrap();
        let entry = entry_path(&cache_root, &repo_root, target);
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&entry, b"not json").unwrap();
        assert!(lookup_in(&cache_root, &repo_root, target).is_none());
    }

    #[test]
    fn version_mismatch_is_a_miss() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        fs::create_dir_all(&repo_root).unwrap();
        let entry = entry_path(&cache_root, &repo_root, target);
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let bad = CacheFile {
            version: CACHE_VERSION + 1,
            repo_root: repo_root.to_string_lossy().into_owned(),
            target: target.to_string(),
            executable_path: "/nope".into(),
            binary_mtime_ns: 0,
            build_witnesses: vec![BuildWitness {
                path: "/nope".into(),
                mtime_ns: 0,
            }],
        };
        fs::write(&entry, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(lookup_in(&cache_root, &repo_root, target).is_none());
    }

    #[test]
    fn different_repo_roots_are_distinct_keys() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let target = "//tools/boss/cli:boss";

        let repo_a = temp.path().join("repo-a");
        let exe_a = repo_a.join("bazel-bin/tools/boss/cli/boss");
        let build_a = repo_a.join("tools/boss/cli/BUILD.bazel");
        touch(&exe_a, "a");
        touch(&build_a, "a");
        record_in(&cache_root, &repo_a, target, &exe_a).expect("record a");

        let repo_b = temp.path().join("repo-b");
        let exe_b = repo_b.join("bazel-bin/tools/boss/cli/boss");
        let build_b = repo_b.join("tools/boss/cli/BUILD.bazel");
        touch(&exe_b, "b");
        touch(&build_b, "b");
        record_in(&cache_root, &repo_b, target, &exe_b).expect("record b");

        assert_eq!(
            lookup_in(&cache_root, &repo_a, target).unwrap(),
            exe_a
        );
        assert_eq!(
            lookup_in(&cache_root, &repo_b, target).unwrap(),
            exe_b
        );
    }
}
