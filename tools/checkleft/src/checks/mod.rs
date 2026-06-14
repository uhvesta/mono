mod api_breaking_surface;
mod bazel;
mod code_patterns;
mod docs_link_integrity;
mod forbidden_imports_deps;
mod forbidden_paths;
mod frontend_no_legacy_api;
mod ifchange_thenchange;
mod repo_visibility;
mod rust_giant_struct_common;
mod rust_giant_struct_instantiation_use_builder;
mod rust_test_rule_coverage;
mod todo_expiry;
mod typo;
mod workflow_action_version;
mod workflow_run_patterns;
mod workflow_shell_strict;

use anyhow::Result;

use crate::check::CheckRegistry;

pub fn register_builtin_checks(registry: &mut CheckRegistry) -> Result<()> {
    registry.register(api_breaking_surface::ApiBreakingSurfaceCheck)?;
    registry.register(bazel::BazelPoliciesCheck)?;
    registry.register(bazel::BazelrcPoliciesCheck)?;
    registry.register(bazel::BazelversionPoliciesCheck)?;
    registry.register(code_patterns::CodePatternsCheck)?;
    registry.register(docs_link_integrity::DocsLinkIntegrityCheck)?;
    registry.register(forbidden_imports_deps::ForbiddenImportsDepsCheck)?;
    registry.register(forbidden_paths::ForbiddenPathsCheck)?;
    registry.register(frontend_no_legacy_api::FrontendNoLegacyApiCheck)?;
    registry.register(ifchange_thenchange::IfChangeThenChangeCheck)?;
    registry.register(repo_visibility::RepoVisibilityCheck)?;
    registry.register(rust_giant_struct_instantiation_use_builder::RustGiantStructInstantiationUseBuilderCheck)?;
    registry.register(rust_test_rule_coverage::RustTestRuleCoverageCheck)?;
    registry.register(todo_expiry::TodoExpiryCheck)?;
    registry.register(typo::TypoCheck)?;
    registry.register(workflow_action_version::WorkflowActionVersionCheck)?;
    registry.register(workflow_run_patterns::WorkflowRunPatternsCheck)?;
    registry.register(workflow_shell_strict::WorkflowShellStrictCheck)?;
    Ok(())
}
