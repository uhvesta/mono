mod bazel_policies;
mod bazelrc_policies;
mod bazelversion_policies;
mod rc_parser;
mod starlark;

pub(crate) use bazel_policies::BazelPoliciesCheck;
pub(crate) use bazelrc_policies::BazelrcPoliciesCheck;
pub(crate) use bazelversion_policies::BazelversionPoliciesCheck;
