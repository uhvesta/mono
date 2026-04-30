use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::app::RepobinError;
use crate::bazel::BazelAdapter;
use crate::config::{RepoConfig, load_repo_config};

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
    let tool =
        repo_config
            .config
            .tools
            .get(tool_name)
            .ok_or_else(|| RepobinError::ToolNotConfigured {
                tool: tool_name.to_string(),
                config_path: repo_config.config_path.clone(),
            })?;

    plan_from_target(
        bazel,
        &repo_config.repo_root,
        tool_name,
        &tool.target,
        cwd,
        forwarded_args,
    )
}

fn plan_from_target<B: BazelAdapter>(
    bazel: &B,
    repo_root: &Path,
    tool_name: &str,
    target: &str,
    cwd: &Path,
    forwarded_args: &[OsString],
) -> Result<DispatchPlan, RepobinError> {
    bazel.build(repo_root, target)?;
    let executable_path = bazel.resolve_executable(repo_root, target)?;

    Ok(DispatchPlan {
        repo_root: repo_root.to_path_buf(),
        tool_name: tool_name.to_string(),
        target: target.to_string(),
        executable_path,
        original_cwd: cwd.to_path_buf(),
        forwarded_args: forwarded_args.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crate::bazel::BazelAdapter;
    use crate::config::{Config, RepoConfig, ToolConfig};

    use super::prepare_dispatch_from_repo_config;

    #[derive(Default)]
    struct FakeBazel {
        builds: RefCell<Vec<(PathBuf, String)>>,
        queries: RefCell<Vec<(PathBuf, String)>>,
        executable: PathBuf,
    }

    impl BazelAdapter for FakeBazel {
        fn build(&self, repo_root: &Path, target: &str) -> Result<(), crate::app::RepobinError> {
            self.builds
                .borrow_mut()
                .push((repo_root.to_path_buf(), target.to_string()));
            Ok(())
        }

        fn resolve_executable(
            &self,
            repo_root: &Path,
            target: &str,
        ) -> Result<PathBuf, crate::app::RepobinError> {
            self.queries
                .borrow_mut()
                .push((repo_root.to_path_buf(), target.to_string()));
            Ok(self.executable.clone())
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
            &[
                std::ffi::OsString::from("task"),
                std::ffi::OsString::from("list"),
            ],
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
}
