import SwiftUI
import Textual

// MARK: - Heading

struct BossHeadingStyle: StructuredText.HeadingStyle {
    // Maps the Boss type scale (26 / 22 / 18 / 16 / 14 / 14 pt) onto
    // SwiftUI's body font (17 pt on macOS) via fontScale so the rendering
    // continues to respond to dynamic-type changes.
    private static let fontScales: [CGFloat] = [
        26.0 / 17.0,
        22.0 / 17.0,
        18.0 / 17.0,
        16.0 / 17.0,
        14.0 / 17.0,
        14.0 / 17.0,
    ]
    private static let weights: [Font.Weight] = [
        .bold, .semibold, .semibold, .semibold, .semibold, .semibold,
    ]

    func makeBody(configuration: Configuration) -> some View {
        let level = min(max(configuration.headingLevel, 1), 6)
        configuration.label
            .textual.fontScale(Self.fontScales[level - 1])
            .textual.lineSpacing(.fontScaled(0.125))
            .textual.blockSpacing(.init(top: 16, bottom: 8))
            .fontWeight(Self.weights[level - 1])
    }
}

extension StructuredText.HeadingStyle where Self == BossHeadingStyle {
    static var boss: Self { .init() }
}

// MARK: - Code block

struct BossCodeBlockStyle: StructuredText.CodeBlockStyle {
    func makeBody(configuration: Configuration) -> some View {
        Overflow {
            configuration.label
                .textual.lineSpacing(.fontScaled(0.225))
                .textual.fontScale(0.85)
                .fixedSize(horizontal: false, vertical: true)
                .monospaced()
                .padding(12)
        }
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
        )
        .textual.blockSpacing(.init(top: 0, bottom: 16))
    }
}

extension StructuredText.CodeBlockStyle where Self == BossCodeBlockStyle {
    static var boss: Self { .init() }
}

// MARK: - Block quote

struct BossBlockQuoteStyle: StructuredText.BlockQuoteStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 0) {
            RoundedRectangle(cornerRadius: 1.5)
                .fill(Color.accentColor.opacity(0.6))
                .frame(width: 3)
            configuration.label
                .foregroundStyle(.secondary)
                .textual.padding(.horizontal, .fontScaled(1))
        }
    }
}

extension StructuredText.BlockQuoteStyle where Self == BossBlockQuoteStyle {
    static var boss: Self { .init() }
}

// MARK: - Table

struct BossTableStyle: StructuredText.TableStyle {
    private static let borderWidth: CGFloat = 0.5
    private static let cornerRadius: CGFloat = 6

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .textual.tableCellSpacing(
                horizontal: Self.borderWidth,
                vertical: Self.borderWidth
            )
            .textual.blockSpacing(.init(top: 0, bottom: 16))
            .textual.tableOverlay { layout in
                Canvas { context, _ in
                    for divider in layout.dividers() {
                        context.fill(
                            Path(divider),
                            with: .style(Color(nsColor: .separatorColor).opacity(0.4))
                        )
                    }
                }
            }
            .padding(Self.borderWidth)
            .overlay(
                RoundedRectangle(cornerRadius: Self.cornerRadius)
                    .stroke(Color(nsColor: .separatorColor), lineWidth: Self.borderWidth)
            )
    }
}

extension StructuredText.TableStyle where Self == BossTableStyle {
    static var boss: Self { .init() }
}

// MARK: - Inline

extension InlineStyle {
    static var boss: InlineStyle {
        InlineStyle()
            .code(
                .font(.system(.callout, design: .monospaced)),
                .backgroundColor(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
            )
            .strong(.fontWeight(.semibold))
            .link(.foregroundColor(.accentColor))
    }
}

// MARK: - Bundle style

struct BossStructuredTextStyle: StructuredText.Style {
    let inlineStyle: InlineStyle = .boss
    let headingStyle: BossHeadingStyle = .boss
    let paragraphStyle: StructuredText.GitHubParagraphStyle = .gitHub
    let blockQuoteStyle: BossBlockQuoteStyle = .boss
    let codeBlockStyle: BossCodeBlockStyle = .boss
    let listItemStyle: StructuredText.DefaultListItemStyle = .default
    let unorderedListMarker: StructuredText.HierarchicalSymbolListMarker =
        .hierarchical(.disc, .circle, .square)
    let orderedListMarker: StructuredText.DecimalListMarker = .decimal
    let tableStyle: BossTableStyle = .boss
    let tableCellStyle: StructuredText.GitHubTableCellStyle = .gitHub
    let thematicBreakStyle: StructuredText.GitHubThematicBreakStyle = .gitHub
}

extension StructuredText.Style where Self == BossStructuredTextStyle {
    static var boss: Self { .init() }
}

// MARK: - Entry point

extension View {
    /// Applies the Boss markdown theme to any `StructuredText` (or
    /// `InlineText`) descendants. Single-line seam — every call site
    /// uses this so the visual language stays coherent.
    func bossMarkdown() -> some View {
        self.textual.structuredTextStyle(BossStructuredTextStyle())
    }
}

#Preview {
    StructuredText(
        markdown: """
            # Boss markdown

            A paragraph with **bold**, *italic*, and `inline code`.

            > A blockquote with the accent-color rail.

            ```swift
            struct Greeter {
                let name: String
            }
            ```

            | Column A | Column B |
            | -------- | -------- |
            | one      | two      |
            """
    )
    .padding()
    .bossMarkdown()
}
