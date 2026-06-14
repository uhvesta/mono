/// Wasm component bytes for the file/size check, embedded at compile time.
/// Lives in its own Bazel library so the generated wasm artifact (which is in
/// compile_data) does not trigger rules_rust's "symlink-sources-to-bazel-out"
/// mode inside the main checkleft_lib target.
pub static WASM: &[u8] = include_bytes!("file_size_check_component.wasm");
