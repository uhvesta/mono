// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "Boss",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "Boss", targets: ["Boss"]),
    ],
    targets: [
        .binaryTarget(
            name: "GhosttyKit",
            path: "ThirdParty/GhosttyKit.xcframework"
        ),
        .executableTarget(
            name: "Boss",
            dependencies: [
                "GhosttyKit",
            ],
            path: "Sources",
            resources: [
                .copy("Resources/TrekIcons"),
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
