import XCTest
@testable import Boss

/// The Product-create form's repo-URL field is intentionally optional —
/// per design Q10 of `multi-repo-work-modeling.md`, a product can span
/// multiple repos, in which case the default is left blank and each
/// work item carries its own repo override. The form surfaces that
/// affordance through `ProductRepoFieldCopy`; this test pins the wording
/// so a UI tweak that drops the "optional" cue or the override-required
/// hint trips a failing test rather than silently shipping a form that
/// looks mandatory.
final class ProductCreateFormCopyTests: XCTestCase {
    /// The placeholder must mark the field as optional so a user
    /// glancing at the sheet doesn't assume it's required. Without the
    /// "(optional)" cue the field reads identical to `Name`, which is
    /// the form's only truly required input.
    func testPlaceholderLabelsFieldAsOptional() {
        XCTAssertEqual(ProductRepoFieldCopy.placeholder, "Remote URL (optional)")
    }

    /// The helper text must do two jobs: tell the user the field is
    /// blank-safe, and tell them what blank *means* downstream — that
    /// per-work-item overrides become required. Dropping either half
    /// makes the form misleading.
    func testHelperTextExplainsBlankAndOverrideRequirement() {
        let text = ProductRepoFieldCopy.helperText
        XCTAssertTrue(
            text.localizedCaseInsensitiveContains("optional"),
            "helper text should call out that the field is optional, got: \(text)"
        )
        XCTAssertTrue(
            text.localizedCaseInsensitiveContains("multiple repos"),
            "helper text should mention multi-repo products, got: \(text)"
        )
        XCTAssertTrue(
            text.localizedCaseInsensitiveContains("per-work-item")
                || text.localizedCaseInsensitiveContains("override"),
            "helper text should explain that per-work-item overrides will be required, got: \(text)"
        )
    }
}
