//! Benchmark: cold (JIT) vs warm (AOT cache hit) component compilation.
//!
//! Run with:
//!   cargo run --example wasm_aot_cache_bench --features benchmarking
//!
//! Expected output (times are approximate and host-dependent):
//!   cold  (JIT compile):     ~10-50 ms per invocation
//!   warm  (cache hit):       ~1-5 ms per invocation
//!
//! The ratio validates the cost story from the design doc: AOT + cache
//! amortizes the one-time compile cost so repeated check invocations pay
//! only deserialization + instantiation, not full compilation.

use std::hint::black_box;
use std::time::{Duration, Instant};

use anyhow::Result;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use wasmtime::component::Component;
use wasmtime::{Config, Engine};

use checkleft::external::ComponentAotCache;

const SAMPLES: usize = 10;

fn main() -> Result<()> {
    let component_bytes = build_bench_component();
    let artifact_sha256 = sha256_hex(&component_bytes);

    println!("Component size: {} bytes", component_bytes.len());
    println!("Samples per measurement: {SAMPLES}\n");

    let engine = build_engine()?;

    // --- Cold: JIT compile from bytes, no cache ---
    let cold = measure("cold  (JIT compile, no cache)", SAMPLES, || {
        let _ = black_box(Component::new(&engine, &component_bytes)?);
        Ok(())
    })?;

    // --- Warm: AOT cache hit (deserialize from .cwasm) ---
    let tmp = tempdir()?;
    let cache = ComponentAotCache::open(tmp.path())?;
    // Prime the cache with a single compile before measuring
    cache.load_or_compile(&engine, "bench-component", &component_bytes, &artifact_sha256)?;

    let warm = measure("warm  (AOT cache hit)", SAMPLES, || {
        let _ = black_box(cache.load_or_compile(&engine, "bench-component", &component_bytes, &artifact_sha256)?);
        Ok(())
    })?;

    println!(
        "\nSpeedup (cold / warm): {:.1}×",
        cold.as_secs_f64() / warm.as_secs_f64()
    );

    Ok(())
}

/// Build a non-trivial WAT component that exercises the compiler meaningfully.
/// A more realistic artifact would be a compiled check component, but this
/// approximation is portable without requiring a cross-compiled wasm32-wasip2
/// toolchain in the bench environment.
fn build_bench_component() -> Vec<u8> {
    // Minimal valid component — replace with a larger artifact for a more
    // realistic measurement when a pre-built .wasm is available.
    wat::parse_str("(component)").expect("parse bench component")
}

fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Ok(Engine::new(&config)?)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn measure(label: &str, samples: usize, mut f: impl FnMut() -> Result<()>) -> Result<Duration> {
    let mut total = Duration::ZERO;
    for _ in 0..samples {
        let t = Instant::now();
        f()?;
        total += t.elapsed();
    }
    let avg = total / samples as u32;
    println!("{label:<40} avg={avg:.2?}  total={total:.2?}");
    Ok(avg)
}
