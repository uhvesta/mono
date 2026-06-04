/// One resolved PR entry in the changelog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub pr_number: u64,
    pub title: String,
    pub author_login: String,
    pub pr_url: String,
}

/// The full changelog for a tag range.
#[derive(Debug, Clone)]
pub struct ChangelogRange {
    pub from_tag: String,
    pub to_tag: String,
    pub compare_url: String,
    pub entries: Vec<ChangelogEntry>,
}
