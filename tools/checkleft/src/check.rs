use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::exclusion::{DeclaredExclusion, ExclusionStatus};
use crate::input::{ChangeSet, SourceTree};
use crate::output::CheckResult;

#[async_trait]
pub trait ConfiguredCheck: Send + Sync {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult>;

    /// Exclusions this configured check honors that are eligible for stale-exclusion
    /// auditing (see [`crate::exclusion`]). Each carries the inputs it depends on;
    /// checkleft re-evaluates an exclusion only when one of those inputs changes in the
    /// diff. The default returns none, so a check opts into auditing simply by
    /// overriding this.
    fn declared_exclusions(&self) -> Vec<DeclaredExclusion> {
        Vec::new()
    }

    /// Re-evaluate a single declared exclusion as if it were not configured, to decide
    /// whether it is still load-bearing. The runner only calls this for exclusions whose
    /// declared dependencies intersect the changeset.
    ///
    /// Implementations must fail safe: when staleness cannot be proven (file unreadable,
    /// ambiguous target, entry not recognized), return [`ExclusionStatus::Unknown`]
    /// rather than guessing [`ExclusionStatus::Stale`]. The default returns `Unknown`.
    async fn evaluate_exclusion(
        &self,
        _exclusion: &DeclaredExclusion,
        _tree: &dyn SourceTree,
    ) -> Result<ExclusionStatus> {
        Ok(ExclusionStatus::Unknown)
    }
}

#[async_trait]
pub trait Check: Send + Sync {
    fn id(&self) -> &str;

    fn description(&self) -> &str;

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>>;

    /// Like `configure`, but also passes the CHECKS.toml directory (repo-root-relative).
    /// Checks that need to scope exclusions to the config subtree should override this.
    /// The default delegates to `configure`, ignoring the scope.
    fn configure_scoped(
        &self,
        config: &toml::Value,
        _config_dir: Option<&Path>,
    ) -> Result<Arc<dyn ConfiguredCheck>> {
        self.configure(config)
    }

    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        self.configure(config)?.run(changeset, tree).await
    }
}

#[derive(Default)]
pub struct CheckRegistry {
    checks: BTreeMap<String, Arc<dyn Check>>,
}

impl CheckRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<C>(&mut self, check: C) -> Result<()>
    where
        C: Check + 'static,
    {
        self.register_arc(Arc::new(check))
    }

    pub fn register_arc(&mut self, check: Arc<dyn Check>) -> Result<()> {
        let id = check.id().to_owned();
        if self.checks.contains_key(&id) {
            bail!("check already registered: {id}");
        }
        self.checks.insert(id, check);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Check>> {
        self.checks.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<dyn Check>> {
        self.checks.values().cloned().collect()
    }
}
