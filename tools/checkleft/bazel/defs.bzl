CheckInfo = provider(
    doc = "Metadata for one repo-local checkleft exec-v1 package.",
    fields = {
        "args": "Static argv entries for the wrapped executable.",
        "check_id": "External package id stored in the generated manifest.",
        "launcher": "Executable File produced by local_check for checkleft to invoke.",
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
    if file.short_path.startswith("../"):
        return "bazel-bin/external/{}".format(file.short_path[len("../"):])
    return "bazel-bin/{}".format(file.short_path)


def _render_bash_runfiles_setup(require_runfiles):
    lines = [
        "self=\"$0\"",
        "if [[ \"$self\" != /* ]]; then",
        "    self=\"$PWD/$self\"",
        "fi",
        "while [[ -L \"$self\" ]]; do",
        "    target=\"$(readlink \"$self\")\"",
        "    if [[ \"$target\" == /* ]]; then",
        "        self=\"$target\"",
        "    else",
        "        self=\"$(cd -- \"$(dirname -- \"$self\")\" && pwd)/$target\"",
        "    fi",
        "done",
        "",
        "runfiles=\"${RUNFILES_DIR:-}\"",
        "if [[ -z \"$runfiles\" && -n \"${RUNFILES_MANIFEST_FILE:-}\" ]]; then",
        "    runfiles=\"${RUNFILES_MANIFEST_FILE}\"",
        "    if [[ \"$runfiles\" == *.runfiles_manifest ]]; then",
        "        runfiles=\"${runfiles%_manifest}\"",
        "    elif [[ \"$runfiles\" == */MANIFEST ]]; then",
        "        runfiles=\"${runfiles%/MANIFEST}\"",
        "    else",
        "        echo \"error: unexpected RUNFILES_MANIFEST_FILE value: $RUNFILES_MANIFEST_FILE\" >&2",
        "        exit 1",
        "    fi",
        "fi",
        "if [[ -z \"$runfiles\" && -e \"$self.runfiles\" ]]; then",
        "    runfiles=\"$self.runfiles\"",
        "fi",
    ]
    if require_runfiles:
        lines.extend([
            "if [[ -z \"$runfiles\" ]]; then",
            "    echo \"error: failed to locate Bazel runfiles for $self\" >&2",
            "    exit 1",
            "fi",
        ])
    lines.extend([
        "if [[ -n \"$runfiles\" ]]; then",
        "    if [[ \"$runfiles\" != /* ]]; then",
        "        runfiles=\"$PWD/$runfiles\"",
        "    fi",
        "    export RUNFILES=\"$runfiles\"",
        "    export RUNFILES_DIR=\"$runfiles\"",
        "fi",
    ])
    return "\n".join(lines)


def _local_check_impl(ctx):
    check_id = ctx.attr.id.strip() or ctx.label.name
    if not check_id:
        fail("`id` must not be empty after defaulting from `name`")

    implementation_name = ctx.attr.implementation_name.strip() or check_id
    if implementation_name.startswith("generated:"):
        fail("`implementation_name` must not include the `generated:` prefix")

    for arg in ctx.attr.exec_args:
        if not arg:
            fail("`args` entries must not be empty")

    launcher = ctx.actions.declare_file(ctx.label.name)
    launcher_script = """#!/usr/bin/env bash
set -euo pipefail

{runfiles_setup}

cd "$RUNFILES"

exec "$runfiles/{workspace_name}/{binary_short_path}" "$@"
""".format(
        runfiles_setup = _render_bash_runfiles_setup(require_runfiles = True),
        workspace_name = ctx.workspace_name,
        binary_short_path = ctx.executable.binary.short_path,
    )
    ctx.actions.write(output = launcher, content = launcher_script, is_executable = True)

    manifest = ctx.actions.declare_file("{}.check.toml".format(ctx.label.name))
    ctx.actions.write(
        output = manifest,
        content = _render_exec_manifest(
            check_id = check_id,
            executable_path = _workspace_bin_path(launcher),
            args = ctx.attr.exec_args,
            provenance_target = str(ctx.attr.binary.label),
        ),
    )

    runfiles = ctx.runfiles(files = [ctx.executable.binary]).merge(
        ctx.attr.binary[DefaultInfo].default_runfiles,
    )

    return [
        DefaultInfo(
            executable = launcher,
            files = depset([launcher, manifest]),
            runfiles = runfiles.merge(ctx.runfiles(files = [manifest])),
        ),
        CheckInfo(
            args = ctx.attr.exec_args,
            check_id = check_id,
            launcher = launcher,
            implementation_name = implementation_name,
            manifest = manifest,
            provenance_target = str(ctx.attr.binary.label),
        ),
    ]


_local_check_rule = rule(
    implementation = _local_check_impl,
    executable = True,
    attrs = {
        "id": attr.string(default = ""),
        "binary": attr.label(
            mandatory = True,
            executable = True,
            cfg = "target",
        ),
        "exec_args": attr.string_list(),
        "implementation_name": attr.string(default = ""),
    },
)


def _local_check_macro_impl(name, visibility, id, binary, args, implementation_name):
    _local_check_rule(
        name = name,
        visibility = visibility,
        id = id,
        binary = binary,
        exec_args = args,
        implementation_name = implementation_name,
    )


local_check = macro(
    attrs = {
        "id": attr.string(default = "", configurable = False),
        "binary": attr.label(
            mandatory = True,
            executable = True,
            cfg = "target",
            configurable = False,
        ),
        "args": attr.string_list(default = [], configurable = False),
        "implementation_name": attr.string(default = "", configurable = False),
    },
    implementation = _local_check_macro_impl,
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
                executable_path = _workspace_bin_path(info.launcher),
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
    external_checks_file_arg = ""
    extra_files = [ctx.executable._checkleft_bin, index_file]
    if ctx.file.external_checks_file != None:
        external_checks_output = ctx.actions.declare_file(
            "{}__{}".format(ctx.label.name, ctx.file.external_checks_file.basename),
        )
        ctx.actions.symlink(
            output = external_checks_output,
            target_file = ctx.file.external_checks_file,
        )
        external_checks_file_arg = " --external-checks-file \"$workspace_dir/{}\"".format(
            _workspace_bin_path(external_checks_output),
        )
        extra_files.append(external_checks_output)

    launcher = ctx.actions.declare_file("{}.sh".format(ctx.label.name))
    script = """#!/usr/bin/env bash
set -euo pipefail

{runfiles_setup}

workspace_dir="${{BUILD_WORKSPACE_DIRECTORY:-$PWD}}"
cd "$workspace_dir"

export CHECKLEFT_EXTERNAL_PROVIDER_MODE=generated-only
export CHECKLEFT_EXTERNAL_CHECK_INDEX="$workspace_dir/{index_path}"

exec "$workspace_dir/{checkleft_path}" run{external_checks_file_arg} "$@"
""".format(
        runfiles_setup = _render_bash_runfiles_setup(require_runfiles = False),
        index_path = _workspace_bin_path(index_file),
        checkleft_path = _workspace_bin_path(ctx.executable._checkleft_bin),
        external_checks_file_arg = external_checks_file_arg,
    )
    ctx.actions.write(output = launcher, content = script, is_executable = True)

    runfiles = ctx.runfiles(files = extra_files).merge(
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
        "external_checks_file": attr.label(
            allow_single_file = True,
        ),
        "_checkleft_bin": attr.label(
            default = "//tools/checkleft:checkleft",
            executable = True,
            cfg = "target",
        ),
    },
)


def _checkleft_impl(name, visibility, check_index, checks, external_checks_file):
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
        external_checks_file = external_checks_file,
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
        "external_checks_file": attr.label(
            allow_single_file = True,
            configurable = False,
        ),
    },
    implementation = _checkleft_impl,
)
