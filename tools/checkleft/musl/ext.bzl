"""Bzlmod extension: register Rust toolchains for musl cross-compilation.

rules_rust's built-in toolchain registration maps both x86_64-unknown-linux-gnu
and x86_64-unknown-linux-musl to identical Bazel platform constraints
(@platforms//cpu:x86_64 + @platforms//os:linux), making them ambiguous when
both are registered in the same hub.  This extension sidesteps the ambiguity:

  1. For each supported exec triple, creates a rust_toolchain_tools_repository
     (downloads the musl std library targeting x86_64/aarch64-unknown-linux-musl)
     and a toolchain_repository_proxy with target_compatible_with that includes
     @zig_sdk//libc:musl (from hermetic_cc_toolchain).

  2. The root MODULE.bazel registers these proxy repos BEFORE @rust_toolchains//:all,
     so they win for //platforms:linux_x86_64_musl (which carries @zig_sdk//libc:musl)
     while the existing GNU toolchain is filtered out for that platform because it
     doesn't carry @zig_sdk//libc:musl in its target_compatible_with.

  3. For the default linux platform (no @zig_sdk//libc:musl constraint), the musl
     toolchains declared here are filtered out, leaving the GNU toolchain in place.
"""

load(
    "@rules_rust//rust:repositories.bzl",
    "rust_toolchain_tools_repository",
    "toolchain_repository_proxy",
)
load(
    "@rules_rust//rust/platform:triple_mappings.bzl",
    "triple_to_constraint_set",
)

# Must match the version in MODULE.bazel rust.toolchain().
_RUST_VERSION = "1.95.0"

# Exec triples for which we register a musl cross-compile toolchain.
# Covers macOS (arm/x86) and Linux (arm/x86) CI agents.
_EXEC_TRIPLES = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
]

# (Rust target triple, libc constraint)
_MUSL_TARGETS = [
    ("x86_64-unknown-linux-musl", "@zig_sdk//libc:musl"),
    ("aarch64-unknown-linux-musl", "@zig_sdk//libc:musl"),
]

def _safe(s):
    return s.replace("-", "_")

def _musl_rust_toolchain_impl(_module_ctx):
    for exec_triple in _EXEC_TRIPLES:
        exec_constraints = triple_to_constraint_set(exec_triple)
        for musl_triple, libc_constraint in _MUSL_TARGETS:
            # target_compatible_with from rules_rust triple mapping PLUS the
            # hermetic_cc_toolchain libc discriminator.
            target_constraints = triple_to_constraint_set(musl_triple) + [libc_constraint]
            base = "rust_musl_{}_{}".format(_safe(exec_triple), _safe(musl_triple))
            tools_name = base + "_tools"
            proxy_name = base

            rust_toolchain_tools_repository(
                name = tools_name,
                exec_triple = exec_triple,
                target_triple = musl_triple,
                version = _RUST_VERSION,
                # hermetic_cc_toolchain (Zig CC) provides its own musl sysroot and
                # CRT files (rcrt1.o, crti.o, crtbeginS.o).  Disable Rust's
                # self-contained startup objects to avoid duplicate symbol errors
                # from the linker (_init, _fini, _start, _start_c).
                extra_rustc_flags = ["-C", "link-self-contained=no"],
            )

            toolchain_repository_proxy(
                name = proxy_name,
                toolchain = "@{}//:rust_toolchain".format(tools_name),
                toolchain_type = "@rules_rust//rust:toolchain_type",
                exec_compatible_with = exec_constraints,
                target_compatible_with = target_constraints,
                target_settings = ["@rules_rust//rust/toolchain/channel:stable"],
            )

musl_rust_toolchain = module_extension(
    implementation = _musl_rust_toolchain_impl,
)
