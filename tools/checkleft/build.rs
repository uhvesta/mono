fn main() {
    // Release pipeline sets CHECKLEFT_VERSION before building.
    // Emit CHECKLEFT_BUILD_VERSION so that option_env!("CHECKLEFT_BUILD_VERSION")
    // in main.rs resolves to the tag version rather than the Cargo.toml placeholder.
    // Dev Cargo builds that don't set CHECKLEFT_VERSION fall through to CARGO_PKG_VERSION.
    if let Ok(v) = std::env::var("CHECKLEFT_VERSION") {
        println!("cargo:rustc-env=CHECKLEFT_BUILD_VERSION={v}");
    }
    println!("cargo:rerun-if-env-changed=CHECKLEFT_VERSION");

    // Emit the wasmtime version so runtime.rs can include it in the .cwasm
    // cache key.  The cache key must change whenever wasmtime is bumped because
    // precompiled .cwasm artifacts are not portable across wasmtime releases.
    // We read from the workspace Cargo.lock (two levels up from this manifest)
    // rather than Cargo.toml because the lock file records the exact resolved
    // version and is guaranteed to exist in the workspace root.
    let wasmtime_version = read_wasmtime_version_from_lock();
    println!("cargo:rustc-env=CHECKLEFT_WASMTIME_VERSION={wasmtime_version}");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
}

fn read_wasmtime_version_from_lock() -> String {
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path).unwrap_or_default();
    extract_wasmtime_version_from_lock(&lock).unwrap_or_else(|| "unknown".to_owned())
}

fn extract_wasmtime_version_from_lock(lock: &str) -> Option<String> {
    let mut in_wasmtime_package = false;
    for line in lock.lines() {
        if line == "[[package]]" {
            in_wasmtime_package = false;
        }
        if line == r#"name = "wasmtime""# {
            in_wasmtime_package = true;
        }
        if in_wasmtime_package
            && let Some(rest) = line.strip_prefix("version = \"")
            && let Some(v) = rest.strip_suffix('"')
        {
            return Some(v.to_owned());
        }
    }
    None
}
