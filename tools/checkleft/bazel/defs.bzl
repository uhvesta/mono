CheckInfo = provider(
    doc = "Metadata for one repo-local checkleft exec-v1 package.",
    fields = {
        "args": "Static argv entries for the wrapped executable.",
        "binary": "Executable File produced by Bazel.",
        "check_id": "External package id stored in the generated manifest.",
        "implementation_name": "Generated implementation id used in check indexes.",
        "manifest": "Manifest file written by local_check.",
        "provenance_target": "Target label recorded in manifest provenance.",
    },
)

CheckIndexInfo = provider(
    doc = "Metadata for a generated checkleft index.",
    fields = {
        "index": "Generated index TOML file.",
    },
)


def _sanitize_fragment(value):
    sanitized = value
    for needle in [" ", "/", "\\", ":", "@", "[", "]", "(", ")", "{", "}", ",", ";", "=", "\"", "'"]:
        sanitized = sanitized.replace(needle, "_")
    if sanitized:
        return sanitized
    return "check"


def _toml_string(value):
    return "\"{}\"".format(
        value.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n"),
    )


def _render_exec_manifest(check_id, executable_path, args, provenance_target):
    lines = [
        "id = {}".format(_toml_string(check_id)),
        "runtime = \"exec-v1\"",
        "api_version = \"v1\"",
        "mode = \"exec\"",
        "executable_path = {}".format(_toml_string(executable_path)),
    ]
    if args:
        lines.append(
            "args = [{}]".format(", ".join([_toml_string(arg) for arg in args])),
        )
    lines.extend([
        "",
        "[provenance]",
        "generator = \"bazel\"",
        "target = {}".format(_toml_string(provenance_target)),
        "",
    ])
    return "\n".join(lines)


def _workspace_bin_path(file):
    return "bazel-bin/{}".format(file.short_path)


def _local_check_impl(ctx):
    check_id = ctx.attr.id.strip() or ctx.label.name
    if not check_id:
        fail("`id` must not be empty after defaulting from `name`")

    implementation_name = ctx.attr.implementation_name.strip() or check_id
    if implementation_name.startswith("generated:"):
        fail("`implementation_name` must not include the `generated:` prefix")

    for arg in ctx.attr.args:
        if not arg:
            fail("`args` entries must not be empty")

    manifest = ctx.actions.declare_file("{}.check.toml".format(ctx.label.name))
    ctx.actions.write(
        output = manifest,
        content = _render_exec_manifest(
            check_id = check_id,
            executable_path = _workspace_bin_path(ctx.executable.binary),
            args = ctx.attr.args,
            provenance_target = str(ctx.attr.binary.label),
        ),
    )

    return [
        DefaultInfo(
            files = depset([manifest]),
            runfiles = ctx.runfiles(files = [manifest, ctx.executable.binary]),
        ),
        CheckInfo(
            args = ctx.attr.args,
            binary = ctx.executable.binary,
            check_id = check_id,
            implementation_name = implementation_name,
            manifest = manifest,
            provenance_target = str(ctx.attr.binary.label),
        ),
    ]


local_check = rule(
    implementation = _local_check_impl,
    attrs = {
        "id": attr.string(default = ""),
        "binary": attr.label(
            mandatory = True,
            executable = True,
            cfg = "target",
        ),
        "args": attr.string_list(),
        "implementation_name": attr.string(default = ""),
    },
)


def _check_index_impl(ctx):
    if not ctx.attr.checks:
        fail("`checks` must contain at least one local_check target")

    entries = []
    generated_manifests = []
    seen = {}

    for index, dep in enumerate(ctx.attr.checks):
        info = dep[CheckInfo]
        if info.implementation_name in seen:
            fail(
                "duplicate generated implementation `{}` from {} and {}".format(
                    info.implementation_name,
                    seen[info.implementation_name],
                    dep.label,
                ),
            )
        seen[info.implementation_name] = dep.label

        manifest = ctx.actions.declare_file(
            "{}_{}_{}.check.toml".format(
                ctx.label.name,
                index,
                _sanitize_fragment(info.implementation_name),
            ),
        )
        ctx.actions.write(
            output = manifest,
            content = _render_exec_manifest(
                check_id = info.check_id,
                executable_path = _workspace_bin_path(info.binary),
                args = info.args,
                provenance_target = info.provenance_target,
            ),
        )
        generated_manifests.append(manifest)
        entries.append((info.implementation_name, manifest.basename))

    index_file = ctx.actions.declare_file("{}.index.toml".format(ctx.label.name))
    lines = ["version = 1", ""]
    for implementation_name, manifest_basename in entries:
        lines.extend([
            "[[packages]]",
            "implementation = {}".format(
                _toml_string("generated:{}".format(implementation_name)),
            ),
            "manifest = {}".format(_toml_string("./{}".format(manifest_basename))),
            "",
        ])
    ctx.actions.write(output = index_file, content = "\n".join(lines))

    runfiles = ctx.runfiles(files = [index_file] + generated_manifests)
    for dep in ctx.attr.checks:
        runfiles = runfiles.merge(dep[DefaultInfo].default_runfiles)

    return [
        DefaultInfo(
            files = depset([index_file]),
            runfiles = runfiles,
        ),
        CheckIndexInfo(index = index_file),
    ]


check_index = rule(
    implementation = _check_index_impl,
    attrs = {
        "checks": attr.label_list(
            mandatory = True,
            providers = [CheckInfo],
        ),
    },
)


def _declare_check_index(name, visibility, checks):
    check_index(
        name = name,
        visibility = visibility,
        checks = checks,
    )


def _checkleft_launcher_impl(ctx):
    index_file = ctx.attr.check_index[CheckIndexInfo].index
    launcher = ctx.actions.declare_file("{}.sh".format(ctx.label.name))
    script = """#!/usr/bin/env bash
set -euo pipefail

workspace_dir="${{BUILD_WORKSPACE_DIRECTORY:-$PWD}}"
cd "$workspace_dir"

export CHECKLEFT_EXTERNAL_PROVIDER_MODE=generated-only
export CHECKLEFT_EXTERNAL_CHECK_INDEX="$workspace_dir/{index_path}"

exec "$workspace_dir/{checkleft_path}" run "$@"
""".format(
        index_path = _workspace_bin_path(index_file),
        checkleft_path = _workspace_bin_path(ctx.executable._checkleft_bin),
    )
    ctx.actions.write(output = launcher, content = script, is_executable = True)

    runfiles = ctx.runfiles(files = [ctx.executable._checkleft_bin, index_file]).merge(
        ctx.attr.check_index[DefaultInfo].default_runfiles,
    )

    return DefaultInfo(
        executable = launcher,
        runfiles = runfiles,
    )


_checkleft_launcher = rule(
    implementation = _checkleft_launcher_impl,
    executable = True,
    attrs = {
        "check_index": attr.label(
            mandatory = True,
            providers = [CheckIndexInfo],
        ),
        "_checkleft_bin": attr.label(
            default = "//tools/checkleft:checkleft",
            executable = True,
            cfg = "target",
        ),
    },
)


def _checkleft_impl(name, visibility, check_index, checks):
    has_check_index = check_index != None
    has_checks = checks != []

    if has_check_index == has_checks:
        fail("exactly one of `check_index` or `checks` must be set")

    resolved_check_index = check_index
    if has_checks:
        resolved_check_index = name + "__embedded_index"
        _declare_check_index(
            name = resolved_check_index,
            visibility = ["//visibility:private"],
            checks = checks,
        )

    _checkleft_launcher(
        name = name,
        visibility = visibility,
        check_index = resolved_check_index,
    )


checkleft = macro(
    attrs = {
        "check_index": attr.label(
            providers = [CheckIndexInfo],
            configurable = False,
        ),
        "checks": attr.label_list(
            default = [],
            providers = [CheckInfo],
            configurable = False,
        ),
    },
    implementation = _checkleft_impl,
)
