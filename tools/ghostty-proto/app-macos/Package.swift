// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "GhosttyProtoApp",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "GhosttyProtoApp", targets: ["GhosttyProtoApp"]),
    ],
    targets: [
        .binaryTarget(
            name: "GhosttyKit",
            path: "ThirdParty/GhosttyKit.xcframework"
        ),
        .executableTarget(
            name: "GhosttyProtoApp",
            dependencies: ["GhosttyKit"],
            path: "Sources",
            linkerSettings: [
                .linkedFramework("Carbon"),
                .linkedLibrary("c++"),
            ]
        ),
    ]
)
