load("@build_bazel_rules_apple//apple:apple.bzl", "apple_static_xcframework_import")

# Exposed as @ghostty_kit//:GhosttyKit — consumed by //tools/boss/app-macos:boss_mac_app_lib.
apple_static_xcframework_import(
    name = "GhosttyKit",
    xcframework_imports = glob(
        ["GhosttyKit.xcframework/**"],
        exclude = ["GhosttyKit.xcframework/**/._*"],
    ),
    # System frameworks required by GhosttyKit's prebuilt static libraries:
    #   Carbon     — KeymapDarwin (TISCopyCurrentKeyboardLayoutInputSource et al.)
    #   GameController — imgui_impl_osx.o (OBJC_CLASS_ references)
    # libc++ is listed explicitly because GhosttyKit ships prebuilt object files;
    # Bazel cannot infer C++ linkage from a binary-only target.
    sdk_frameworks = [
        "Carbon",
        "GameController",
    ],
    sdk_dylibs = ["c++"],
    visibility = ["@@//tools/boss/app-macos:__pkg__"],
)
