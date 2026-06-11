use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use checkleft::config::ConfigResolver;
use tempfile::tempdir;

const SIBLING_DIRS: usize = 200;
const FILES_PER_DIR: usize = 50;
const ROOT_CHECKS: usize = 20;
const BACKEND_CHECKS: usize = 20;
const SAMPLES: usize = 5;

fn main() -> Result<()> {
    let temp = tempdir()?;
    seed_fixture(temp.path())?;
    let file_paths = build_file_paths();

    let uncached = measure("uncached", SAMPLES, || {
        let resolver = ConfigResolver::new(temp.path())?;
        resolve_all_without_cache(&resolver, &file_paths)
    })?;
    let cached_cold = measure("cached-cold", SAMPLES, || {
        let resolver = ConfigResolver::new(temp.path())?;
        resolve_all_cached(&resolver, &file_paths)
    })?;
    let cached_hot = measure_with_setup("cached-hot", SAMPLES, || {
        let resolver = ConfigResolver::new(temp.path())?;
        resolve_all_cached(&resolver, &file_paths)?;
        let file_paths = file_paths.clone();
        Ok(move || resolve_all_cached(&resolver, &file_paths))
    })?;

    println!(
        "workload: {SIBLING_DIRS} sibling dirs x {FILES_PER_DIR} files = {} file resolutions",
        file_paths.len()
    );
    println!("config chain: root ({ROOT_CHECKS} checks) -> backend ({BACKEND_CHECKS} checks)");
    println!();
    println!("{:<12} {:>12} {:>12}", "mode", "median", "speedup");
    println!(
        "{:<12} {:>12} {:>12}",
        uncached.label,
        format_duration(uncached.median),
        "1.00x"
    );
    println!(
        "{:<12} {:>12} {:>11.2}x",
        cached_cold.label,
        format_duration(cached_cold.median),
        uncached.median.as_secs_f64() / cached_cold.median.as_secs_f64()
    );
    println!(
        "{:<12} {:>12} {:>11.2}x",
        cached_hot.label,
        format_duration(cached_hot.median),
        uncached.median.as_secs_f64() / cached_hot.median.as_secs_f64()
    );

    Ok(())
}

fn seed_fixture(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join("backend"))?;
    fs::write(root.join("CHECKS.toml"), checks_file("root-check", ROOT_CHECKS))?;
    fs::write(
        root.join("backend/CHECKS.toml"),
        checks_file("backend-check", BACKEND_CHECKS),
    )?;

    for dir_index in 0..SIBLING_DIRS {
        let dir = root.join(format!("backend/service-{dir_index:03}"));
        fs::create_dir_all(&dir)?;
        for file_index in 0..FILES_PER_DIR {
            fs::write(dir.join(format!("file-{file_index:03}.rs")), "fn main() {}\n")?;
        }
    }

    Ok(())
}

fn checks_file(prefix: &str, count: usize) -> String {
    let mut output = String::new();
    for check_index in 0..count {
        output.push_str(&format!(
            r#"[[checks]]
id = "{prefix}-{check_index:03}"

[checks.config]
max_lines = {}

"#,
            100 + check_index
        ));
    }
    output
}

fn build_file_paths() -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(SIBLING_DIRS * FILES_PER_DIR);
    for dir_index in 0..SIBLING_DIRS {
        for file_index in 0..FILES_PER_DIR {
            paths.push(PathBuf::from(format!(
                "backend/service-{dir_index:03}/file-{file_index:03}.rs"
            )));
        }
    }
    paths
}

fn resolve_all_cached(resolver: &ConfigResolver, file_paths: &[PathBuf]) -> Result<u64> {
    let mut total_enabled = 0u64;
    for path in file_paths {
        let resolved = resolver.resolve_for_file(path)?;
        total_enabled += black_box(resolved.enabled().count() as u64);
    }
    Ok(total_enabled)
}

fn resolve_all_without_cache(resolver: &ConfigResolver, file_paths: &[PathBuf]) -> Result<u64> {
    let mut total_enabled = 0u64;
    for path in file_paths {
        let resolved = resolver.resolve_for_file_without_cache(path)?;
        total_enabled += black_box(resolved.enabled().count() as u64);
    }
    Ok(total_enabled)
}

fn measure<F>(label: &'static str, samples: usize, mut run: F) -> Result<Measurement>
where
    F: FnMut() -> Result<u64>,
{
    let mut durations = Vec::with_capacity(samples);
    let mut sink = 0u64;

    for _ in 0..samples {
        let started_at = Instant::now();
        sink ^= run()?;
        durations.push(started_at.elapsed());
    }

    durations.sort_unstable();
    black_box(sink);

    Ok(Measurement {
        label,
        median: durations[durations.len() / 2],
    })
}

fn measure_with_setup<Setup, Run>(label: &'static str, samples: usize, mut setup: Setup) -> Result<Measurement>
where
    Setup: FnMut() -> Result<Run>,
    Run: FnMut() -> Result<u64>,
{
    let mut durations = Vec::with_capacity(samples);
    let mut sink = 0u64;

    for _ in 0..samples {
        let mut run = setup()?;
        let started_at = Instant::now();
        sink ^= run()?;
        durations.push(started_at.elapsed());
    }

    durations.sort_unstable();
    black_box(sink);

    Ok(Measurement {
        label,
        median: durations[durations.len() / 2],
    })
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 1 {
        format!("{:.2}s", duration.as_secs_f64())
    } else if duration.as_millis() >= 1 {
        format!("{:.1}ms", duration.as_secs_f64() * 1_000.0)
    } else {
        format!("{:.1}us", duration.as_secs_f64() * 1_000_000.0)
    }
}

struct Measurement {
    label: &'static str,
    median: Duration,
}
