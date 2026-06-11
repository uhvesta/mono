use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use git_changelog::{
    ChangelogRenderer, ExtractionConfig, GithubMarkdownRenderer, derive_paths_from_project, extract_changelog,
    repo_slug_from_remote,
};

#[derive(Debug, Parser)]
#[command(name = "changelog", about = "Path-scoped GitHub-matching changelog generator")]
struct Cli {
    /// Start tag (exclusive lower bound).
    #[arg(long)]
    from: String,

    /// End tag or ref (defaults to HEAD).
    #[arg(long, default_value = "HEAD")]
    to: String,

    /// Include only commits that touch files matching this glob (repeatable).
    #[arg(long = "path", name = "glob")]
    paths: Vec<String>,

    /// File containing glob patterns (one per line; '#' comments ok).
    #[arg(long)]
    paths_file: Option<PathBuf>,

    /// PROJECT.yaml whose directory (implicit) and `paths` entries define owned paths.
    /// May be combined with --path / --paths-file; all sources are unioned.
    #[arg(long)]
    project: Option<PathBuf>,

    /// GitHub repo slug `owner/name`. Derived from git remote `origin` if omitted.
    #[arg(long)]
    repo: Option<String>,

    /// Enrich PR titles and author logins via `gh` API (requires GITHUB_TOKEN / gh auth).
    #[arg(long)]
    enrich: bool,

    /// Path to the git repository (defaults to the current directory).
    #[arg(long, default_value = ".")]
    git_dir: PathBuf,
}

fn run(cli: Cli) -> Result<()> {
    let repo_path = cli.git_dir.canonicalize().context("invalid --git-dir")?;

    let mut globs = cli.paths;

    if let Some(paths_file) = cli.paths_file {
        let content = fs::read_to_string(&paths_file)
            .with_context(|| format!("could not read --paths-file {}", paths_file.display()))?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            globs.push(line.to_string());
        }
    }

    if let Some(project_file) = cli.project {
        let project_globs = derive_paths_from_project(&project_file, &repo_path)
            .with_context(|| format!("could not derive paths from --project {}", project_file.display()))?;
        globs.extend(project_globs);
    }

    let repo_slug = match cli.repo {
        Some(slug) => slug,
        None => repo_slug_from_remote(&repo_path, "origin")
            .context("could not derive repo slug from origin remote; use --repo")?,
    };

    let config = ExtractionConfig {
        repo_path,
        from_tag: cli.from,
        to_tag: cli.to,
        path_globs: globs,
        repo_slug,
        enrich: cli.enrich,
    };

    let range = extract_changelog(&config)?;
    let output = GithubMarkdownRenderer.render(&range);

    io::stdout()
        .write_all(output.as_bytes())
        .context("failed to write to stdout")?;
    println!();

    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
