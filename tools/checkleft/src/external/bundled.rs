//! First-party check definitions embedded directly in the checkleft binary.
//!
//! A target repo that has *no* checkleft definition files on disk can still run
//! these checks: the manifests are compiled into the binary via `include_str!`
//! (declarative) or `include_bytes!` (component-mode), so there is zero install.
//! A check whose `id` (or `check`) names a bundled definition resolves to it
//! automatically — no `implementation:` line required.
//!
//! ## Adding a bundled declarative definition
//!
//! 1. Add the manifest at `tools/checkleft/checks/<namespace>/<name>.yaml`.
//! 2. Add a `BundledCheckDef::declarative` entry to [`BUNDLED_CHECK_DEFS`] below.
//! 3. Add the file to `checkleft_lib`'s `compile_data` in `BUILD.bazel` so the
//!    bazel build can read it at compile time.
//!
//! ## Adding a bundled component definition
//!
//! 1. Build the `.wasm` component artifact via the `rust_wasm_component` bazel
//!    rule (T9) and record its sha256.
//! 2. Add a `BundledCheckDef::component` entry to [`BUNDLED_CHECK_DEFS`] below,
//!    using `include_bytes!` for the artifact.
//! 3. Add the artifact file to `checkleft_lib`'s `compile_data` in `BUILD.bazel`.
//!
//! We embed each file explicitly (rather than `include_dir!`) because the bazel
//! build does not run `build.rs`, and every embedded file must be declared as
//! `compile_data` anyway — so an explicit, reviewable table is both hermetic
//! under bazel and clearer about exactly what ships in the binary.

use sha2::{Digest, Sha256};

use anyhow::{Context, Result};

use super::{
    EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_COMPONENT_RUNTIME_V1, ExternalCheckComponentLimits,
    ExternalCheckComponentPackage, ExternalCheckImplementationRef, ExternalCheckPackage,
    ExternalCheckPackageImplementation, ExternalCheckPackageProvider, parse_external_check_manifest,
};

/// A first-party definition compiled into the binary.
struct BundledCheckDef {
    /// All check names this definition exports. For declarative defs this is
    /// exactly one entry — the bundle key (directory name). For component defs
    /// it lists every check name the component exports (must match `list-checks`).
    check_names: &'static [&'static str],
    kind: BundledCheckDefKind,
    /// Per-execution resource limits for component-mode definitions.
    /// `None` uses the host's defaults (5 s timeout, 256 MiB memory).
    limits: Option<ExternalCheckComponentLimits>,
}

enum BundledCheckDefKind {
    /// A declarative YAML manifest (`include_str!`-embedded at compile time).
    Declarative {
        /// File extension of the embedded manifest (`yaml`/`yml`), selecting the
        /// parser for `parse_external_check_manifest`.
        extension: &'static str,
        /// The raw manifest contents, embedded at compile time.
        contents: &'static str,
    },
    /// A WebAssembly Component Model artifact (`include_bytes!`-embedded at
    /// compile time). Each entry in `check_names` corresponds to one export of
    /// the component and resolves to a distinct logical package.
    Component {
        /// Raw wasm component bytes, embedded at compile time via `include_bytes!`.
        bytes: &'static [u8],
    },
}

/// The embedded first-party definitions. To add one, see the module docs.
static BUNDLED_CHECK_DEFS: &[BundledCheckDef] = &[
    BundledCheckDef {
        check_names: &["format/bazel"],
        kind: BundledCheckDefKind::Declarative {
            extension: "yaml",
            contents: include_str!("../../checks/format/bazel.yaml"),
        },
        limits: None,
    },
    BundledCheckDef {
        check_names: &["format/rust"],
        kind: BundledCheckDefKind::Declarative {
            extension: "yaml",
            contents: include_str!("../../checks/format/rust.yaml"),
        },
        limits: None,
    },
    BundledCheckDef {
        check_names: &["lint/rust"],
        kind: BundledCheckDefKind::Declarative {
            extension: "yaml",
            contents: include_str!("../../checks/lint/rust.yaml"),
        },
        limits: None,
    },
    BundledCheckDef {
        check_names: &["lint/bazel"],
        kind: BundledCheckDefKind::Declarative {
            extension: "yaml",
            contents: include_str!("../../checks/lint/bazel.yaml"),
        },
        limits: None,
    },
    BundledCheckDef {
        check_names: &["file/size"],
        kind: BundledCheckDefKind::Component {
            bytes: checkleft_file_size_wasm_bundle::WASM,
        },
        limits: None,
    },
    BundledCheckDef {
        check_names: &["rust/giant-structs"],
        kind: BundledCheckDefKind::Component {
            // Bytes come from the checkleft_wasm_bundle micro-library so the
            // generated wasm artifact lives in that target's compile_data, not
            // in checkleft_lib's.  That separation keeps checkleft_lib in
            // "source mode" and preserves CARGO_MANIFEST_DIR for bindgen!.
            bytes: checkleft_wasm_bundle::WASM,
        },
        // No explicit timeout: uses the proportional default
        // (BASE_COMPONENT_TIMEOUT_MS + PER_FILE_COMPONENT_TIMEOUT_MS × n_files),
        // which scales naturally with whole-repo changesets without over-budgeting
        // small PRs.
        limits: None,
    },
];

/// Names of all bundled definitions (for diagnostics / `--list`-style output).
pub fn bundled_check_names() -> impl Iterator<Item = &'static str> {
    BUNDLED_CHECK_DEFS
        .iter()
        .flat_map(|def| def.check_names.iter().copied())
}

/// Resolves [`ExternalCheckImplementationRef::Bundled`] references against the
/// definitions embedded in the binary. Always available — needs no on-disk
/// files, env vars, or network — which is the whole point of the bundle.
#[derive(Debug, Default)]
pub struct BundledExternalCheckPackageProvider;

impl ExternalCheckPackageProvider for BundledExternalCheckPackageProvider {
    fn resolve(&self, implementation_ref: &ExternalCheckImplementationRef) -> Result<Option<ExternalCheckPackage>> {
        let ExternalCheckImplementationRef::Bundled(name) = implementation_ref else {
            return Ok(None);
        };

        resolve_from_defs(BUNDLED_CHECK_DEFS, name)
    }
}

/// Look up a check by name across `defs`, returning the appropriate package.
fn resolve_from_defs(defs: &[BundledCheckDef], name: &str) -> Result<Option<ExternalCheckPackage>> {
    for def in defs {
        if !def.check_names.contains(&name) {
            continue;
        }
        return Ok(Some(match &def.kind {
            BundledCheckDefKind::Declarative { extension, contents } => {
                parse_external_check_manifest(contents, extension)
                    .with_context(|| format!("invalid bundled check definition `{name}`"))?
            }
            BundledCheckDefKind::Component { bytes } => {
                build_bundled_component_package(name, bytes, def.limits.clone())
            }
        }));
    }
    Ok(None)
}

/// Build an [`ExternalCheckPackage`] for a single check exported by a bundled
/// component. The sha256 is computed from the embedded bytes at call time; for
/// bundled-in-binary bytes this is a deterministic integrity check.
fn build_bundled_component_package(
    check_name: &str,
    bytes: &'static [u8],
    limits: Option<ExternalCheckComponentLimits>,
) -> ExternalCheckPackage {
    let hash = Sha256::digest(bytes);
    let mut sha256 = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(&mut sha256, "{byte:02x}");
    }
    ExternalCheckPackage {
        id: check_name.to_owned(),
        runtime: EXTERNAL_CHECK_COMPONENT_RUNTIME_V1.to_owned(),
        api_version: EXTERNAL_CHECK_API_V1.to_owned(),
        implementation: ExternalCheckPackageImplementation::Component(ExternalCheckComponentPackage {
            artifact_path: String::new(),
            artifact_sha256: sha256,
            artifact_bytes: Some(bytes),
            check_name: check_name.to_owned(),
            limits,
            checks: None,
            provenance: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bundled_format_bazel_definition() {
        let provider = BundledExternalCheckPackageProvider;
        let package = provider
            .resolve(&ExternalCheckImplementationRef::Bundled("format/bazel".to_owned()))
            .expect("resolve")
            .expect("package");
        assert_eq!(package.id, "format/bazel");
    }

    #[test]
    fn every_bundled_definition_parses() {
        // Guards against a stale `include_str!` row: each embedded manifest must
        // parse cleanly so a target repo never hits a broken bundled def.
        let provider = BundledExternalCheckPackageProvider;
        for name in bundled_check_names() {
            provider
                .resolve(&ExternalCheckImplementationRef::Bundled(name.to_owned()))
                .unwrap_or_else(|err| panic!("bundled def `{name}` failed to parse: {err:#}"))
                .unwrap_or_else(|| panic!("bundled def `{name}` did not resolve"));
        }
    }

    #[test]
    fn returns_none_for_unknown_bundled_name() {
        let provider = BundledExternalCheckPackageProvider;
        let resolved = provider
            .resolve(&ExternalCheckImplementationRef::Bundled("does-not-exist".to_owned()))
            .expect("resolve");
        assert!(resolved.is_none());
    }

    #[test]
    fn ignores_non_bundled_refs() {
        let provider = BundledExternalCheckPackageProvider;
        let resolved = provider
            .resolve(&ExternalCheckImplementationRef::Generated("buildifier".to_owned()))
            .expect("resolve");
        assert!(resolved.is_none());
    }

    #[test]
    fn component_def_resolves_each_check_name_to_separate_package() {
        let fake_bytes: &'static [u8] = b"\x00asm\x01\x00\x00\x00"; // not valid wasm, but sufficient for resolver tests
        let defs = [BundledCheckDef {
            check_names: &["check-alpha", "check-beta"],
            kind: BundledCheckDefKind::Component { bytes: fake_bytes },
            limits: None,
        }];

        for expected_name in ["check-alpha", "check-beta"] {
            let pkg = resolve_from_defs(&defs, expected_name)
                .expect("resolve")
                .unwrap_or_else(|| panic!("expected package for `{expected_name}`"));

            assert_eq!(pkg.id, expected_name);
            assert_eq!(pkg.runtime, EXTERNAL_CHECK_COMPONENT_RUNTIME_V1);

            let ExternalCheckPackageImplementation::Component(comp) = pkg.implementation else {
                panic!("expected Component implementation for `{expected_name}`");
            };
            assert_eq!(comp.check_name, expected_name);
            assert_eq!(comp.artifact_bytes, Some(fake_bytes));
            assert!(!comp.artifact_sha256.is_empty(), "sha256 must be computed");
        }
    }

    #[test]
    fn component_def_returns_none_for_non_exported_name() {
        let defs = [BundledCheckDef {
            check_names: &["check-alpha"],
            kind: BundledCheckDefKind::Component { bytes: b"dummy" },
            limits: None,
        }];

        let result = resolve_from_defs(&defs, "check-gamma").expect("resolve");
        assert!(result.is_none());
    }

    #[test]
    fn bundled_check_names_includes_component_checks() {
        // If there were a component def in BUNDLED_CHECK_DEFS, its check names
        // would appear. Here we test the helper directly with a custom def slice.
        let defs = [
            BundledCheckDef {
                check_names: &["decl-check"],
                kind: BundledCheckDefKind::Declarative {
                    extension: "yaml",
                    contents: "",
                },
                limits: None,
            },
            BundledCheckDef {
                check_names: &["comp-check-a", "comp-check-b"],
                kind: BundledCheckDefKind::Component { bytes: b"x" },
                limits: None,
            },
        ];
        let names: Vec<&str> = defs.iter().flat_map(|d| d.check_names.iter().copied()).collect();
        assert_eq!(names, ["decl-check", "comp-check-a", "comp-check-b"]);
    }

    #[test]
    fn bundled_component_sha256_is_deterministic() {
        let bytes: &'static [u8] = b"test-component-bytes";
        let pkg1 = build_bundled_component_package("my-check", bytes, None);
        let pkg2 = build_bundled_component_package("my-check", bytes, None);
        let ExternalCheckPackageImplementation::Component(c1) = pkg1.implementation else {
            panic!();
        };
        let ExternalCheckPackageImplementation::Component(c2) = pkg2.implementation else {
            panic!();
        };
        assert_eq!(c1.artifact_sha256, c2.artifact_sha256);
        assert_eq!(c1.artifact_sha256.len(), 64);
        assert!(
            c1.artifact_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "sha256 must be hex: {}",
            c1.artifact_sha256
        );
    }

    #[test]
    fn bundled_giant_structs_check_uses_proportional_timeout_by_default() {
        // The bundled check must NOT carry an explicit timeout_ms so the runtime
        // applies the proportional default (BASE + PER_FILE × n_files) rather
        // than a flat limit that over-budgets small PRs and under-budgets large ones.
        let provider = BundledExternalCheckPackageProvider;
        let pkg = provider
            .resolve(&ExternalCheckImplementationRef::Bundled(
                "rust/giant-structs".to_owned(),
            ))
            .expect("resolve")
            .expect("package must exist");
        let ExternalCheckPackageImplementation::Component(comp) = pkg.implementation else {
            panic!("expected Component implementation");
        };
        // limits == None means the runtime uses the proportional formula.
        assert!(
            comp.limits.is_none() || comp.limits.as_ref().is_some_and(|l| l.timeout_ms.is_none()),
            "bundled check must not set an explicit timeout_ms; got: {:?}",
            comp.limits,
        );
    }
}
