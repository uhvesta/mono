/// Wasm component bytes for the consolidated preinstalled check bundle, embedded
/// at compile time. Lives in its own Bazel library so the generated wasm artifact
/// (which is in compile_data) does not trigger rules_rust's
/// "symlink-sources-to-bazel-out" mode inside the main checkleft_lib target.
/// That mode shifts CARGO_MANIFEST_DIR away from the source tree, breaking
/// wasmtime::component::bindgen!'s `path:` resolution for check.wit.
pub static WASM: &[u8] = include_bytes!("preinstalled_bundle_component.wasm");
