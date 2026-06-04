pub mod extract;
pub mod model;
pub mod project;
pub mod render;

pub use extract::{extract_changelog, repo_slug_from_remote, ExtractionConfig};
pub use model::{ChangelogEntry, ChangelogRange};
pub use project::derive_paths_from_project;
pub use render::{ChangelogRenderer, GithubMarkdownRenderer};
