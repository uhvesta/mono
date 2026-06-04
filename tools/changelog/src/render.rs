use crate::model::ChangelogRange;

pub trait ChangelogRenderer {
    fn render(&self, range: &ChangelogRange) -> String;
}

/// Renders a changelog that matches GitHub's auto-generated release notes format.
pub struct GithubMarkdownRenderer;

impl ChangelogRenderer for GithubMarkdownRenderer {
    fn render(&self, range: &ChangelogRange) -> String {
        let mut out = String::new();
        out.push_str("## What's Changed\n");
        for entry in &range.entries {
            out.push_str(&format!(
                "* {} by @{} in {}\n",
                entry.title, entry.author_login, entry.pr_url,
            ));
        }
        // Two blank lines before the Full Changelog line.
        out.push_str("\n\n");
        out.push_str(&format!(
            "**Full Changelog**: {}",
            range.compare_url,
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChangelogEntry;

    #[test]
    fn render_matches_github_format() {
        let range = ChangelogRange {
            from_tag: "v1.0.0".to_string(),
            to_tag: "v1.1.0".to_string(),
            compare_url:
                "https://github.com/example/repo/compare/v1.0.0...v1.1.0".to_string(),
            entries: vec![
                ChangelogEntry {
                    pr_number: 42,
                    title: "Add cool feature".to_string(),
                    author_login: "alice".to_string(),
                    pr_url: "https://github.com/example/repo/pull/42".to_string(),
                },
                ChangelogEntry {
                    pr_number: 41,
                    title: "Fix nasty bug".to_string(),
                    author_login: "bob".to_string(),
                    pr_url: "https://github.com/example/repo/pull/41".to_string(),
                },
            ],
        };

        let renderer = GithubMarkdownRenderer;
        let output = renderer.render(&range);

        let expected = "\
## What's Changed
* Add cool feature by @alice in https://github.com/example/repo/pull/42
* Fix nasty bug by @bob in https://github.com/example/repo/pull/41


**Full Changelog**: https://github.com/example/repo/compare/v1.0.0...v1.1.0";

        assert_eq!(output, expected);
    }
}
