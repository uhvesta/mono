"""Starlark platform transition for building checkleft for musl targets."""

def _musl_x86_64_transition_impl(_settings, _attr):
    return {"//command_line_option:platforms": str(Label("//platforms:linux_x86_64_musl"))}

musl_x86_64_transition = transition(
    implementation = _musl_x86_64_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

def _musl_binary_impl(ctx):
    # Forward the transitioned binary's output files.
    # The binary is cross-compiled for Linux musl; it cannot run on the host
    # (macOS), so we don't declare it executable here — use `bazel build` to
    # obtain the binary in bazel-bin/.
    bin_info = ctx.attr.binary[0][DefaultInfo]
    return [
        DefaultInfo(
            files = bin_info.files,
        ),
    ]

musl_binary = rule(
    implementation = _musl_binary_impl,
    attrs = {
        "binary": attr.label(
            mandatory = True,
            cfg = musl_x86_64_transition,
        ),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
