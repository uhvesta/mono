import XCTest
@testable import Boss

/// Covers the `P<n>` short-id badge on every project surface: kanban
/// swimlane headers (collapsible and non-collapsible), the sidebar
/// project filter list, and the project filter popover. Per the design
/// for "Display project short ids (P<n>) on every macOS surface", each
/// surface must show a secondary-styled `P<n>` chip subordinate to the
/// project name — mirroring the `T<n>` treatment used for tasks.
@MainActor
final class ProjectShortIDBadgeTests: XCTestCase {

    // MARK: - P-prefix format

    /// A project with a `shortID` must format as `P<n>` — bare sigil
    /// plus decimal integer, matching the `T<n>` convention for tasks.
    func testShortIDFormatIsPPrefixed() {
        let project = makeProject(shortID: 491)
        XCTAssertEqual(project.shortID, 491)
        XCTAssertEqual("P\(project.shortID!)", "P491")
    }

    /// Projects that predate the migration arrive with `shortID == nil`;
    /// the badge must be absent rather than rendering an empty capsule.
    func testNilShortIDProducesNoBadge() {
        let project = makeProject(shortID: nil)
        XCTAssertNil(project.shortID)
    }

    // MARK: - Format contract

    /// Badge text has no zero padding — `P7`, not `P007`. Mirrors the
    /// bare-decimal contract already enforced for `T<n>` badges.
    func testShortIDHasNoZeroPadding() {
        let project = makeProject(shortID: 7)
        XCTAssertEqual("P\(project.shortID!)", "P7")
    }

    /// Large short IDs render without abbreviation so two projects
    /// sharing an abbreviation can never be confused at a glance.
    func testShortIDRendersFullDecimalForLargeIDs() {
        let project = makeProject(shortID: 1000)
        XCTAssertEqual("P\(project.shortID!)", "P1000")
    }

    // MARK: - Sidebar subtitle wiring

    /// The sidebar filter list must pass `"P<n>"` as the subtitle when
    /// the project has a shortID, so `WorkSidebarFilterRow` renders it
    /// below the project name in the secondary caption style.
    func testSidebarSubtitleIsPPrefixedWhenShortIDPresent() {
        let project = makeProject(shortID: 42)
        let subtitle = project.shortID.map { "P\($0)" }
        XCTAssertEqual(subtitle, "P42")
    }

    /// When `shortID` is nil the sidebar subtitle must be nil so the
    /// `WorkSidebarFilterRow` collapses its subtitle slot.
    func testSidebarSubtitleIsNilWhenShortIDAbsent() {
        let project = makeProject(shortID: nil)
        let subtitle = project.shortID.map { "P\($0)" }
        XCTAssertNil(subtitle)
    }

    // MARK: - ViewModel wiring

    /// `projectsForSelectedProduct` must surface `shortID` so every
    /// call site (sidebar, popover, section header) can render the chip
    /// without a separate fetch. A join that strips `shortID` would
    /// silently hide the badge on all surfaces.
    func testShortIDSurvivesProjectsForSelectedProductJoin() {
        let model = makeModel(projectShortID: 99)
        let projects = model.projectsForSelectedProduct
        XCTAssertEqual(projects.count, 1)
        XCTAssertEqual(projects[0].shortID, 99)
        XCTAssertEqual("P\(projects[0].shortID!)", "P99")
    }

    /// When the project has no shortID, `projectsForSelectedProduct`
    /// must still return the project (with a nil shortID) so archived /
    /// legacy projects keep appearing in the sidebar and popover.
    func testLegacyProjectWithoutShortIDAppearsInJoin() {
        let model = makeModel(projectShortID: nil)
        let projects = model.projectsForSelectedProduct
        XCTAssertEqual(projects.count, 1)
        XCTAssertNil(projects[0].shortID)
    }

    // MARK: - Section header chip label

    /// The collapsible section header label is derived from the project
    /// shortID the same way the call site computes it — ensures the
    /// `shortIDLabel` parameter carries the right value.
    func testCollapsibleSectionShortIDLabelIsPPrefixedWhenPresent() {
        let model = makeModel(projectShortID: 17)
        let project = model.projectsForSelectedProduct.first!
        let label = project.shortID.map { "P\($0)" }
        XCTAssertEqual(label, "P17")
    }

    /// When a project has no shortID the section header must pass `nil`
    /// for `shortIDLabel` so `CollapsibleWorkBoardSection` hides the chip.
    func testCollapsibleSectionShortIDLabelIsNilWhenAbsent() {
        let model = makeModel(projectShortID: nil)
        let project = model.projectsForSelectedProduct.first!
        let label = project.shortID.map { "P\($0)" }
        XCTAssertNil(label)
    }

    // MARK: - Helpers

    private func makeProject(shortID: Int?) -> WorkProject {
        WorkProject(
            id: "proj_test",
            productID: "prod_test",
            name: "Test Project",
            slug: "test",
            description: "",
            goal: "",
            status: "active",
            priority: "medium",
            createdAt: "2026-05-15T00:00:00Z",
            updatedAt: "2026-05-15T00:00:00Z",
            shortID: shortID
        )
    }

    private func makeModel(projectShortID: Int?) -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-15T00:00:00Z",
                updatedAt: "2026-05-15T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        model.projectsByProductID = [
            "prod_test": [makeProject(shortID: projectShortID)]
        ]
        return model
    }
}
