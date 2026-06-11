use std::env;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::app::RepobinError;
use crate::bazel::BazelAdapter;
use crate::cache::cache_root_from_env;
use crate::config::{RepoConfig, load_repo_config};
use crate::dispatch_cache;

const TRACE_ENV: &str = "REPOBIN_TRACE";
const NO_CACHE_ENV: &str = "REPOBIN_NO_DISPATCH_CACHE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchPlan {
    pub repo_root: PathBuf,
    pub tool_name: String,
    pub target: String,
    pub executable_path: PathBuf,
    pub original_cwd: PathBuf,
    pub forwarded_args: Vec<OsString>,
}

pub fn prepare_dispatch<B: BazelAdapter>(
    bazel: &B,
    cwd: &Path,
    tool_name: &str,
    forwarded_args: &[OsString],
) -> Result<DispatchPlan, RepobinError> {
    let repo_config = load_repo_config(cwd)?;
    prepare_dispatch_from_repo_config(bazel, repo_config, cwd, tool_name, forwarded_args)
}

pub fn prepare_dispatch_from_repo_config<B: BazelAdapter>(
    bazel: &B,
    repo_config: RepoConfig,
    cwd: &Path,
    tool_name: &str,
    forwarded_args: &[OsString],
) -> Result<DispatchPlan, RepobinError> {
    let tool = repo_config
        .config
        .tools
        .get(tool_name)
        .ok_or_else(|| RepobinError::ToolNotConfigured {
            tool: tool_name.to_string(),
            config_path: repo_config.config_path.clone(),
        })?;

    let cache_root = effective_cache_root();
    plan_from_target(
        bazel,
        cache_root.as_deref(),
        &repo_config.repo_root,
        tool_name,
        &tool.target,
        cwd,
        forwarded_args,
    )
}

fn plan_from_target<B: BazelAdapter>(
    bazel: &B,
    cache_root: Option<&Path>,
    repo_root: &Path,
    tool_name: &str,
    target: &str,
    cwd: &Path,
    forwarded_args: &[OsString],
) -> Result<DispatchPlan, RepobinError> {
    if let Some(root) = cache_root
        && let Some(executable_path) = dispatch_cache::lookup_in(root, repo_root, target)
    {
        trace(format_args!(
            "dispatch-cache hit target={target} repo_root={}",
            repo_root.display()
        ));
        return Ok(DispatchPlan {
            repo_root: repo_root.to_path_buf(),
            tool_name: tool_name.to_string(),
            target: target.to_string(),
            executable_path,
            original_cwd: cwd.to_path_buf(),
            forwarded_args: forwarded_args.to_vec(),
        });
    }

    trace(format_args!(
        "dispatch-cache miss target={target} repo_root={} (running bazel build + cquery)",
        repo_root.display()
    ));
    bazel.build(repo_root, target)?;
    let executable_path = bazel.resolve_executable(repo_root, target)?;

    if let Some(root) = cache_root {
        let source_files = match bazel.resolve_source_files(repo_root, target) {
            Ok(files) => files,
            Err(error) => {
                trace(format_args!("dispatch-cache source query failed: {error}"));
                Vec::new()
            }
        };
        if let Err(error) = dispatch_cache::record_in(root, repo_root, target, &executable_path, &source_files) {
            trace(format_args!("dispatch-cache record failed: {error}"));
        }
    }

    Ok(DispatchPlan {
        repo_root: repo_root.to_path_buf(),
        tool_name: tool_name.to_string(),
        target: target.to_string(),
        executable_path,
        original_cwd: cwd.to_path_buf(),
        forwarded_args: forwarded_args.to_vec(),
    })
}

fn effective_cache_root() -> Option<PathBuf> {
    if env::var_os(NO_CACHE_ENV).is_some() {
        return None;
    }
    cache_root_from_env().ok()
}

fn trace(args: std::fmt::Arguments<'_>) {
    if env::var_os(TRACE_ENV).is_none() {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "repobin: {args}");
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use crate::bazel::BazelAdapter;
    use crate::config::{Config, RepoConfig, ToolConfig};

    use super::{plan_from_target, prepare_dispatch_from_repo_config};

    #[derive(Default)]
    struct FakeBazel {
        builds: RefCell<Vec<(PathBuf, String)>>,
        queries: RefCell<Vec<(PathBuf, String)>>,
        executable: PathBuf,
        source_files: Vec<PathBuf>,
    }

    impl BazelAdapter for FakeBazel {
        fn build(&self, repo_root: &Path, target: &str) -> Result<(), crate::app::RepobinError> {
            self.builds
                .borrow_mut()
                .push((repo_root.to_path_buf(), target.to_string()));
            Ok(())
        }

        fn resolve_executable(&self, repo_root: &Path, target: &str) -> Result<PathBuf, crate::app::RepobinError> {
            self.queries
                .borrow_mut()
                .push((repo_root.to_path_buf(), target.to_string()));
            Ok(self.executable.clone())
        }

        fn resolve_source_files(
            &self,
            _repo_root: &Path,
            _target: &str,
        ) -> Result<Vec<PathBuf>, crate::app::RepobinError> {
            Ok(self.source_files.clone())
        }
    }

    fn sample_repo_config() -> RepoConfig {
        RepoConfig {
            repo_root: PathBuf::from("/repo"),
            config_path: PathBuf::from("/repo/REPOBIN.toml"),
            config: Config {
                version: 1,
                tools: BTreeMap::from([(
                    "boss".to_string(),
                    ToolConfig {
                        target: "//tools/boss/cli:boss".to_string(),
                    },
                )]),
            },
        }
    }

    #[test]
    fn prepare_dispatch_builds_and_resolves_target() {
        let bazel = FakeBazel {
            executable: PathBuf::from("/repo/bazel-bin/tools/boss/cli/boss"),
            ..FakeBazel::default()
        };

        let plan = prepare_dispatch_from_repo_config(
            &bazel,
            sample_repo_config(),
            Path::new("/repo/subdir"),
            "boss",
            &[std::ffi::OsString::from("task"), std::ffi::OsString::from("list")],
        )
        .expect("dispatch plan");

        assert_eq!(plan.tool_name, "boss");
        assert_eq!(plan.target, "//tools/boss/cli:boss");
        assert_eq!(plan.original_cwd, Path::new("/repo/subdir"));
        assert_eq!(
            bazel.builds.borrow().as_slice(),
            &[(PathBuf::from("/repo"), "//tools/boss/cli:boss".to_string())]
        );
        assert_eq!(
            bazel.queries.borrow().as_slice(),
            &[(PathBuf::from("/repo"), "//tools/boss/cli:boss".to_string())]
        );
    }

    #[test]
    fn warm_dispatch_skips_bazel_calls() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");
        let build = repo_root.join("tools/boss/cli/BUILD.bazel");

        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        fs::create_dir_all(build.parent().unwrap()).unwrap();
        fs::write(&build, b"rust_binary(...)\n").unwrap();

        let bazel = FakeBazel {
            executable: exe.clone(),
            ..FakeBazel::default()
        };

        let cold_plan = plan_from_target(&bazel, Some(&cache_root), &repo_root, "boss", target, &repo_root, &[])
            .expect("cold plan");
        assert_eq!(cold_plan.executable_path, exe);
        assert_eq!(bazel.builds.borrow().len(), 1);
        assert_eq!(bazel.queries.borrow().len(), 1);

        let warm_plan = plan_from_target(&bazel, Some(&cache_root), &repo_root, "boss", target, &repo_root, &[])
            .expect("warm plan");
        assert_eq!(warm_plan.executable_path, exe);
        // Counts must not increase on the warm hit.
        assert_eq!(
            bazel.builds.borrow().len(),
            1,
            "warm dispatch must not invoke bazel build"
        );
        assert_eq!(
            bazel.queries.borrow().len(),
            1,
            "warm dispatch must not invoke bazel cquery"
        );
    }

    #[test]
    fn build_mtime_advance_invalidates_dispatch_cache() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");
        let build = repo_root.join("tools/boss/cli/BUILD.bazel");

        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        fs::create_dir_all(build.parent().unwrap()).unwrap();
        fs::write(&build, b"rust_binary(...)\n").unwrap();

        let bazel = FakeBazel {
            executable: exe.clone(),
            ..FakeBazel::default()
        };

        plan_from_target(&bazel, Some(&cache_root), &repo_root, "boss", target, &repo_root, &[]).expect("first plan");
        assert_eq!(bazel.builds.borrow().len(), 1);

        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&build)
            .unwrap()
            .set_modified(later)
            .unwrap();

        plan_from_target(&bazel, Some(&cache_root), &repo_root, "boss", target, &repo_root, &[])
            .expect("invalidated plan");
        assert_eq!(
            bazel.builds.borrow().len(),
            2,
            "BUILD mtime advance should miss cache and re-run bazel"
        );
    }

    #[test]
    fn corrupt_cache_falls_through_without_error() {
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");

        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"#!/bin/sh\n").unwrap();

        // Pre-seed a corrupt cache entry at the expected location.
        let entry_dir = cache_root.join("dispatch");
        fs::create_dir_all(&entry_dir).unwrap();
        // We don't know the hash; the corrupt-cache path is exhaustively
        // covered in dispatch_cache::tests. Here we verify that an unrelated
        // garbage file in the cache subdir does not break the slow path.
        fs::write(entry_dir.join("garbage.json"), b"\xff\x00not json").unwrap();

        let bazel = FakeBazel {
            executable: exe.clone(),
            ..FakeBazel::default()
        };
        let plan = plan_from_target(
            &bazel,
            Some(&cache_root),
            &repo_root,
            "boss",
            target,
            &repo_root,
            &[OsString::from("--help")],
        )
        .expect("dispatch plan");
        assert_eq!(plan.executable_path, exe);
        assert_eq!(bazel.builds.borrow().len(), 1);
    }

    #[test]
    fn disabled_cache_runs_bazel_every_time() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");
        let build = repo_root.join("tools/boss/cli/BUILD.bazel");

        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        fs::create_dir_all(build.parent().unwrap()).unwrap();
        fs::write(&build, b"rust_binary(...)\n").unwrap();

        let bazel = FakeBazel {
            executable: exe.clone(),
            ..FakeBazel::default()
        };

        for _ in 0..3 {
            plan_from_target(&bazel, None, &repo_root, "boss", target, &repo_root, &[]).expect("plan");
        }
        assert_eq!(bazel.builds.borrow().len(), 3);
        assert_eq!(bazel.queries.borrow().len(), 3);
    }

    #[test]
    fn source_change_invalidates_dispatch_cache_no_bazel_on_hit() {
        // Regression for: .rs edits with BUILD.bazel unchanged must miss the cache,
        // and a back-to-back dispatch after rebuild must NOT invoke bazel again.
        let temp = TempDir::new().unwrap();
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let target = "//tools/boss/cli:boss";
        let exe = repo_root.join("bazel-bin/tools/boss/cli/boss");
        let build = repo_root.join("tools/boss/cli/BUILD.bazel");
        let src = repo_root.join("tools/boss/cli/src/main.rs");

        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        fs::create_dir_all(build.parent().unwrap()).unwrap();
        fs::write(&build, b"rust_binary(...)\n").unwrap();
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, b"fn main() {}\n").unwrap();

        let bazel = FakeBazel {
            executable: exe.clone(),
            source_files: vec![src.clone()],
            ..FakeBazel::default()
        };

        // Cold dispatch: triggers build + cquery + source query.
        plan_from_target(&bazel, Some(&cache_root), &repo_root, "boss", target, &repo_root, &[]).expect("cold plan");
        assert_eq!(bazel.builds.borrow().len(), 1, "cold: one bazel build");

        // Warm dispatch: no source change → must not call bazel.
        plan_from_target(&bazel, Some(&cache_root), &repo_root, "boss", target, &repo_root, &[]).expect("warm plan");
        assert_eq!(bazel.builds.borrow().len(), 1, "warm hit must not invoke bazel build");
        assert_eq!(bazel.queries.borrow().len(), 1, "warm hit must not invoke bazel cquery");

        // Source file mtime advances (simulates editing a .rs file; BUILD.bazel untouched).
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(later)
            .unwrap();

        // Dispatch after source change: must miss and rebuild.
        plan_from_target(&bazel, Some(&cache_root), &repo_root, "boss", target, &repo_root, &[])
            .expect("invalidated plan");
        assert_eq!(
            bazel.builds.borrow().len(),
            2,
            "source change with unchanged BUILD.bazel must trigger bazel rebuild"
        );

        // Immediately after rebuild: warm hit again, no extra bazel call.
        plan_from_target(&bazel, Some(&cache_root), &repo_root, "boss", target, &repo_root, &[]).expect("re-warm plan");
        assert_eq!(
            bazel.builds.borrow().len(),
            2,
            "dispatch right after rebuild must be a cache hit"
        );
    }
}
