// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "Boss",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "Boss", targets: ["Boss"]),
    ],
    dependencies: [
        .package(url: "https://github.com/gonzalezreal/textual", from: "0.3.1"),
    ],
    targets: [
        .binaryTarget(
            name: "GhosttyKit",
            path: "ThirdParty/GhosttyKit.xcframework"
        ),
        .executableTarget(
            name: "Boss",
            dependencies: [
                .product(name: "Textual", package: "textual"),
                "GhosttyKit",
            ],
            path: "Sources",
            resources: [
                .copy("Resources/TrekIcons"),
            ],
            swiftSettings: [
                // Required so @inlinable code in the Textual library (e.g.
                // .textual.textSelection) expands to the real implementation
                // rather than the no-op #else branch.
                .define("TEXTUAL_ENABLE_TEXT_SELECTION"),
            ],
            linkerSettings: [
                .linkedFramework("Carbon"),
                .linkedLibrary("c++"),
            ]
        ),
        .testTarget(
            name: "BossTests",
            dependencies: ["Boss"],
            path: "Tests/BossTests"
        ),
    ]
)
