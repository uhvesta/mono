import os.log
import SwiftUI

/// Records absolute time at process entry — used to compute interactive_ms
/// in each layer pane.
let processStartTime = Date.now

let renderLog = Logger(subsystem: "com.boss.textualperf", category: "Render")

@main
struct TextualPerfLayeredApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
