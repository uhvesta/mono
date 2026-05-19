// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "textual-perf-layered",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "textualperflayered", targets: ["TextualPerfLayered"]),
    ],
    dependencies: [
        .package(url: "https://github.com/gonzalezreal/textual", from: "0.3.1"),
    ],
    targets: [
        .executableTarget(
            name: "TextualPerfLayered",
            dependencies: [
                .product(name: "Textual", package: "textual"),
            ],
            path: "Sources/TextualPerfLayered",
            exclude: ["Resources/Info.plist"]
        ),
    ]
)
