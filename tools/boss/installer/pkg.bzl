"""
Starlark rules for building the Boss installer artifacts.

Defines four rules:
  boss_pkg_payload        — extracts Boss.app.zip into a staged payload directory
  boss_pkg_unsigned       — runs pkgbuild to produce Boss-<sha>.pkg (unsigned)
  build_info_rs           — emits a Rust source file with stamped build constants
  boss_short_version_plist — emits a plist fragment with both CFBundleShortVersionString
                             (full STABLE_BOSS_VERSION, e.g. "1.0.4-dev-f3be785") and
                             CFBundleVersion (numeric STABLE_BOSS_BASE_VERSION, e.g.
                             "1.0.4") so the About panel shows the full version while
                             keeping CFBundleVersion Apple-compliant.
"""

# ── boss_pkg_payload ──────────────────────────────────────────────────────────

def _boss_pkg_payload_impl(ctx):
    """Extracts the Boss.app archive into a pkgbuild payload directory."""
    output_dir = ctx.actions.declare_directory(ctx.label.name)
    app_archive = ctx.file.app

    ctx.actions.run_shell(
        inputs = [app_archive],
        outputs = [output_dir],
        command = "unzip -q " + app_archive.path + " -d " + output_dir.path,
        mnemonic = "ExtractBossApp",
        progress_message = "Staging Boss.app payload",
    )

    return [DefaultInfo(files = depset([output_dir]))]

boss_pkg_payload = rule(
    implementation = _boss_pkg_payload_impl,
    attrs = {
        "app": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "The Boss.app archive (.zip) from //tools/boss/app-macos:Boss.",
        ),
    },
    doc = """
Extracts the Boss.app.zip archive from macos_application into a directory
suitable for use as the --root argument to pkgbuild.

After extraction the directory contains Boss.app/ with all Contents/ including
the bundled binaries in Contents/Resources/bin/.
""",
)

# ── boss_pkg_unsigned ─────────────────────────────────────────────────────────

def _boss_pkg_unsigned_impl(ctx):
    """Runs pkgbuild + productbuild to produce an unsigned Boss-<sha>.pkg."""
    output_dir = ctx.actions.declare_directory(ctx.label.name)

    # Payload directory (the extracted Boss.app tree)
    payload = ctx.file.payload

    # Distribution XML — enables currentUserHomeDirectory domain so Installer
    # uses the runtime user's ~/Applications rather than a build-time-expanded
    # absolute path.
    distribution = ctx.file.distribution

    # Pre/postinstall scripts directory — all scripts live in the same dir
    scripts = ctx.files.scripts
    if not scripts:
        fail("boss_pkg_unsigned: scripts attribute must not be empty")
    scripts_dir = scripts[0].dirname

    # ctx.info_file is the non-volatile status file (stable-status.txt), which
    # contains STABLE_* keys.  ctx.version_file is the volatile file and does
    # not carry STABLE_ keys.
    info_file = ctx.info_file

    # Build the shell command.  We avoid .format() to sidestep brace-escaping
    # issues with awk; string concatenation is clearer here.
    #
    # Two-step build mirrors release.sh:
    #   1. pkgbuild → BossComponent.pkg  (component package, --install-location /Applications)
    #   2. productbuild --distribution   → Boss-<sha>.pkg  (distribution package)
    #
    # Using /Applications (not ~/Applications) avoids baking the builder's $HOME
    # into the component metadata.  productbuild + distribution.xml with
    # enable_currentUserHome="true" remaps /Applications → ~/Applications at
    # install time on the target machine.
    command = (
        "set -euo pipefail\n" +
        "SHA=$(grep STABLE_BOSS_GIT_SHA " + info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "[ -z \"$SHA\" ] && SHA=unknown\n" +
        # Step 1: component package (intermediate; named to match distribution.xml pkg-ref)
        "/usr/bin/pkgbuild \\\n" +
        "    --root " + payload.path + " \\\n" +
        "    --identifier dev.spinyfin.boss.installer \\\n" +
        "    --install-location /Applications \\\n" +
        "    --scripts " + scripts_dir + " \\\n" +
        "    --version \"0+${SHA}\" \\\n" +
        "    " + output_dir.path + "/BossComponent.pkg\n" +
        # Step 2: distribution package wrapping the component with domain metadata
        "/usr/bin/productbuild \\\n" +
        "    --distribution " + distribution.path + " \\\n" +
        "    --package-path " + output_dir.path + " \\\n" +
        "    " + output_dir.path + "/Boss-${SHA}.pkg\n"
    )

    ctx.actions.run_shell(
        inputs = [payload, distribution, info_file] + scripts,
        outputs = [output_dir],
        command = command,
        mnemonic = "BossPkgBuild",
        progress_message = "Building unsigned Boss installer .pkg",
        # pkgbuild/productbuild are macOS-only and must run locally
        execution_requirements = {"local": "1"},
    )

    return [DefaultInfo(files = depset([output_dir]))]

boss_pkg_unsigned = rule(
    implementation = _boss_pkg_unsigned_impl,
    attrs = {
        "payload": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "The staged payload directory from boss_pkg_payload.",
        ),
        "scripts": attr.label(
            mandatory = True,
            allow_files = True,
            doc = "Filegroup of pre/postinstall scripts for pkgbuild --scripts.",
        ),
        "distribution": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "distribution.xml for productbuild — sets currentUserHomeDirectory domain.",
        ),
    },
    doc = """
Builds an unsigned .pkg installer from the staged payload directory.

Output: a directory boss_pkg_unsigned/ containing Boss-<sha>.pkg where <sha>
is STABLE_BOSS_GIT_SHA from the stable workspace status file (ctx.info_file).
Also contains BossComponent.pkg (intermediate component package).

Two-step build: pkgbuild produces BossComponent.pkg with --install-location
/Applications; productbuild wraps it with distribution.xml which sets
enable_currentUserHome="true" so Installer.app resolves /Applications relative
to the runtime user's home, not the builder's $HOME.

The .pkg installs Boss.app to ~/Applications (currentUserHomeDirectory domain,
no admin rights required).  It is unsigned; run release.sh (chore 2) to sign,
notarize, and staple the final artifact.
""",
)

# ── build_info_rs ─────────────────────────────────────────────────────────────

def _build_info_rs_impl(ctx):
    """Emits a Rust source file with stamped build constants."""
    output = ctx.actions.declare_file(ctx.attr.out)
    # ctx.info_file is the non-volatile status file (stable-status.txt)
    info_file = ctx.info_file

    # Read ONLY STABLE_BOSS_BASE_VERSION (the numeric, tag-derived version, e.g.
    # "1.0.4") from stable-status.txt and emit a Rust source file with pub
    # constants. BOSS_GIT_SHA and BOSS_BUILD_TIME are intentionally emitted as
    # the literal "unknown" rather than stamped.
    #
    # Why not stamp the SHA / build time here: this file is compiled into
    # engine_lib (and the cli/bossctl crates). The git SHA changes on every
    # commit and a wall-clock build time changes on every build, so stamping
    # either one made the generated file change constantly, which busted the
    # Rust action cache and forced a full recompile of the largest crate in the
    # tree on every CI run. Reading only the base version — which changes only
    # when a new boss-v* release tag is cut — keeps this file byte-stable across
    # commits, so the downstream rustc actions hit the disk cache.
    #
    # The runtime "which binary am I?" signal that actually matters lives in
    # engine::build_info::binary_fingerprint() (a hash of the engine binary's
    # own bytes), and the user-facing release version is stamped separately into
    # Info.plist by boss_short_version_plist. So nothing of value is lost here.
    command = (
        "set -euo pipefail\n" +
        "VERSION=$(grep STABLE_BOSS_BASE_VERSION " + info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "[ -z \"$VERSION\" ] && VERSION=unknown\n" +
        "printf 'pub const BOSS_VERSION: &str = \"%s\";\\n" +
        "#[allow(dead_code)]\\n" +
        "pub const BOSS_GIT_SHA: &str = \"unknown\";\\n" +
        "#[allow(dead_code)]\\n" +
        "pub const BOSS_BUILD_TIME: &str = \"unknown\";\\n' " +
        "\"$VERSION\" > " + output.path + "\n"
    )

    ctx.actions.run_shell(
        inputs = [info_file],
        outputs = [output],
        command = command,
        mnemonic = "BuildInfoRs",
        progress_message = "Generating build_info_generated.rs",
    )

    return [DefaultInfo(files = depset([output]))]

build_info_rs = rule(
    implementation = _build_info_rs_impl,
    attrs = {
        "out": attr.string(
            mandatory = True,
            doc = "Output filename for the generated Rust source file.",
        ),
    },
    doc = """
Generates a Rust source file containing build constants.

Emits:
  pub const BOSS_VERSION: &str = "<base-version>";   // e.g. "1.0.4"
  pub const BOSS_GIT_SHA: &str = "unknown";          // intentionally not stamped
  pub const BOSS_BUILD_TIME: &str = "unknown";       // intentionally not stamped

BOSS_VERSION is the numeric, tag-derived base version (STABLE_BOSS_BASE_VERSION),
consumed by engine_lib, boss, and bossctl for --version output. It changes only
when a new boss-v* release tag is cut, so this file stays byte-stable across
commits and the crates that compile it in keep hitting the Bazel action cache.

BOSS_GIT_SHA and BOSS_BUILD_TIME are emitted as the literal "unknown" on purpose:
stamping per-commit / per-build values here forced a full recompile of engine_lib
on every CI build. The reliable runtime build identity is
engine::build_info::binary_fingerprint(); the user-facing release version is
stamped separately into Info.plist by boss_short_version_plist.
""",
)

# ── boss_short_version_plist ──────────────────────────────────────────────────

def _boss_short_version_plist_impl(ctx):
    """Emits a plist fragment with both version keys stamped from workspace status."""
    output = ctx.actions.declare_file(ctx.attr.out)
    info_file = ctx.info_file

    # Read STABLE_BOSS_VERSION (full, e.g. "1.0.4-dev-f3be785") and
    # STABLE_BOSS_BASE_VERSION (numeric-only, e.g. "1.0.4") from stable-status.txt.
    #
    # CFBundleVersion and CFBundleShortVersionString must be period-separated
    # non-negative integers (plisttool enforces Apple's requirement). The full
    # version including the "-dev-<sha>" suffix goes in the custom BossFullVersion
    # key; the macOS app reads it via Bundle.main and passes it to
    # orderFrontStandardAboutPanel(options:) so the About panel shows the complete
    # version string. When not stamped, fallbacks are "0.0.0" / "dev" respectively.
    command = (
        "set -euo pipefail\n" +
        "V=$(grep STABLE_BOSS_VERSION " + info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "B=$(grep STABLE_BOSS_BASE_VERSION " + info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "[ -z \"$V\" ] && V=dev\n" +
        "[ -z \"$B\" ] && B=0.0.0\n" +
        "printf '<?xml version=\"1.0\" encoding=\"UTF-8\"?>\\n" +
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\"" +
        " \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\\n" +
        "<plist version=\"1.0\"><dict>\\n" +
        "<key>CFBundleShortVersionString</key><string>%s</string>\\n" +
        "<key>CFBundleVersion</key><string>%s</string>\\n" +
        "<key>BossFullVersion</key><string>%s</string>\\n" +
        "</dict></plist>\\n' \"$B\" \"$B\" \"$V\" > " + output.path + "\n"
    )

    ctx.actions.run_shell(
        inputs = [info_file],
        outputs = [output],
        command = command,
        mnemonic = "BossShortVersionPlist",
        progress_message = "Generating Boss version plist",
    )

    return [DefaultInfo(files = depset([output]))]

boss_short_version_plist = rule(
    implementation = _boss_short_version_plist_impl,
    attrs = {
        "out": attr.string(
            mandatory = True,
            doc = "Output filename for the generated plist fragment.",
        ),
    },
    doc = """
Generates a minimal Info.plist fragment with version keys stamped from stable-status.txt:
  CFBundleShortVersionString = STABLE_BOSS_BASE_VERSION (e.g. "1.0.4")  — numeric only
  CFBundleVersion            = STABLE_BOSS_BASE_VERSION (e.g. "1.0.4")  — numeric only
  BossFullVersion            = STABLE_BOSS_VERSION      (e.g. "1.0.4-dev-f3be785")

CFBundleVersion and CFBundleShortVersionString must be numeric-only (plisttool enforces
Apple's requirement). The full version including the "-dev-<sha>" dev suffix goes in
BossFullVersion (custom key, not validated); the macOS app reads it at runtime and
passes it to orderFrontStandardAboutPanel(options:) so the About panel shows the
complete version string on dev builds.
""",
)
