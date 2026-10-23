import OSLog
import SwiftUI
import WidgetKit

private let logger = Logger(subsystem: "io.github.ashigirl96.ccswap.widget", category: "timeline")

struct Entry: TimelineEntry {
    let date: Date
    let feed: Feed?
    let stale: Bool
}

struct Provider: TimelineProvider {
    func placeholder(in context: Context) -> Entry {
        Entry(date: .now, feed: nil, stale: false)
    }

    func getSnapshot(in context: Context, completion: @escaping (Entry) -> Void) {
        let feed = SharedFiles.loadFeed()
        completion(Entry(date: .now, feed: feed, stale: feed.map { $0.staleDate <= .now } ?? false))
    }

    /// The stale entry is scheduled up front so the widget flags a stopped
    /// menu bar even when no reload happens.
    func getTimeline(in context: Context, completion: @escaping (Timeline<Entry>) -> Void) {
        let now = Date.now
        let feed = SharedFiles.loadFeed()
        logger.notice("timeline reload, feed written_at=\(feed?.writtenAt ?? 0, privacy: .public)")
        var entries = [Entry(date: now, feed: feed, stale: feed.map { $0.staleDate <= now } ?? false)]
        if let feed, feed.staleDate > now {
            entries.append(Entry(date: feed.staleDate, feed: feed, stale: true))
        }
        completion(Timeline(entries: entries, policy: .after(now.addingTimeInterval(60))))
    }
}

private enum Palette {
    static let background = Color(red: 0.122, green: 0.122, blue: 0.122)
    static let text = Color(red: 0.925, green: 0.918, blue: 0.898)
    static let secondary = Color(red: 0.561, green: 0.553, blue: 0.533)
    static let divider = Color(red: 0.173, green: 0.173, blue: 0.173)
    static let track = Color(red: 0.090, green: 0.157, blue: 0.239)
    static let emptyTrack = Color(red: 0.173, green: 0.173, blue: 0.173)
    static let blue = Color(red: 0.184, green: 0.482, blue: 0.898)
    static let activeText = Color(red: 0.435, green: 0.659, blue: 0.961)
    static let amber = Color(red: 0.878, green: 0.627, blue: 0.188)
    static let yellow = Color(red: 1.0, green: 0.839, blue: 0.039)
    static let red = Color(red: 0.898, green: 0.282, blue: 0.302)
    static let redText = Color(red: 0.949, green: 0.467, blue: 0.478)
}

private extension Feed.Window {
    var fill: Color { pct >= 90 ? Palette.red : pct >= 70 ? Palette.yellow : Palette.blue }
}

private extension Feed.Account {
    var session: Feed.Window? { windows.first { $0.name == "Current session" } }
    var limited: Bool { (session?.pct ?? 0) >= 100 }
}

struct UsageBar: View {
    let window: Feed.Window

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(alignment: .firstTextBaseline) {
                Text(window.name).font(.system(size: 13)).lineLimit(1)
                Spacer(minLength: 4)
                Text("\(Int(window.pct.rounded()))%")
                    .font(.system(size: 12).monospacedDigit())
                    .foregroundStyle(window.pct >= 100 ? Palette.redText : Palette.secondary)
            }
            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule().fill(window.pct == 0 ? Palette.emptyTrack : window.fill.opacity(0.2))
                    Capsule().fill(window.fill)
                        .frame(width: proxy.size.width * min(window.pct, 100) / 100)
                }
            }
            .frame(height: 7)
            if let reset = window.resetDate, reset > .now {
                (Text("Resets in ") + Text(reset, style: .relative))
                    .font(.system(size: 11))
                    .lineLimit(1)
                    .foregroundStyle(Palette.secondary)
            }
        }
    }
}

struct AccountColumn: View {
    let column: Feed.Column
    private var account: Feed.Account { column.account }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(account.label).font(.system(size: 16, weight: .semibold)).lineLimit(1)
                    if column.provider == .codex {
                        Text("Codex").font(.system(size: 11)).foregroundStyle(Palette.secondary)
                    }
                }
                status
            }
            if account.disabled {
                Text("Skipped by auto-switch. Enable it again from the menu bar.")
                    .font(.system(size: 12))
                    .foregroundStyle(Palette.secondary)
            } else if let note = account.note {
                Text(note).font(.system(size: 12)).foregroundStyle(Palette.secondary)
            } else {
                ForEach(account.windows.prefix(3)) { UsageBar(window: $0) }
            }
            Spacer(minLength: 0)
            action
        }
        .opacity(account.disabled ? 0.5 : 1)
    }

    @ViewBuilder private var status: some View {
        if account.active {
            Text("In use").font(.system(size: 12)).foregroundStyle(Palette.activeText)
        } else if account.disabled {
            Text("Disabled").font(.system(size: 12)).foregroundStyle(Palette.secondary)
        } else if account.limited {
            Text("Limit reached").font(.system(size: 12)).foregroundStyle(Palette.redText)
        } else {
            Text(" ").font(.system(size: 12))
        }
    }

    @ViewBuilder private var action: some View {
        if account.active {
            Text("In use")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Palette.activeText)
                .frame(maxWidth: .infinity, minHeight: 32)
                .background(RoundedRectangle(cornerRadius: 8).fill(Palette.track))
        } else if !account.disabled {
            Button(intent: SwitchAccountIntent(number: account.number, provider: column.provider)) {
                switchLabel
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(account.limited ? Palette.secondary : .white)
                    .frame(maxWidth: .infinity, minHeight: 32)
                    .background(
                        RoundedRectangle(cornerRadius: 8)
                            .fill(account.limited ? Color.clear : Palette.blue)
                            .strokeBorder(account.limited ? Palette.divider : Palette.blue)
                    )
            }
            .buttonStyle(.plain)
        }
    }

    private var switchLabel: Text {
        if account.limited, let reset = account.session?.resetDate, reset > .now {
            return Text("Switch (back in ") + Text(reset, style: .timer) + Text(")")
        }
        return Text("Switch")
    }
}

struct CcswapWidgetView: View {
    let entry: Entry

    var body: some View {
        Group {
            if let feed = entry.feed {
                VStack(alignment: .leading, spacing: 10) {
                    if entry.stale {
                        (Text("Menu bar isn't running. Updated ") + Text(feed.writtenDate, style: .relative) + Text(" ago."))
                            .font(.system(size: 11))
                            .foregroundStyle(Palette.amber)
                            .padding(.horizontal, 14)
                    }
                    columns(feed)
                }
            } else {
                Text("No usage yet. Start the menu bar with: ccswap menubar")
                    .font(.system(size: 13))
                    .foregroundStyle(Palette.secondary)
            }
        }
        .padding(.vertical, 18)
        .padding(.horizontal, 6)
        .foregroundStyle(Palette.text)
        .containerBackground(Palette.background, for: .widget)
    }

    /// Claude keeps its four columns until Codex needs room; then the row
    /// widens to five and Codex keeps at least two.
    private func shownColumns(_ feed: Feed) -> [Feed.Column] {
        func activeFirst(_ provider: Feed.ProviderKind) -> [Feed.Column] {
            let accounts = feed.accounts(of: provider)
            return (accounts.filter(\.active) + accounts.filter { !$0.active })
                .map { Feed.Column(provider: provider, account: $0) }
        }
        let claude = activeFirst(.claude)
        let codex = activeFirst(.codex)
        if codex.isEmpty {
            return Array(claude.prefix(4))
        }
        let codexShown = codex.prefix(max(2, 5 - min(claude.count, 4)))
        return Array(claude.prefix(min(4, 5 - codexShown.count))) + codexShown
    }

    private func columns(_ feed: Feed) -> some View {
        let shown = shownColumns(feed)
        return HStack(alignment: .top, spacing: 0) {
            ForEach(Array(shown.enumerated()), id: \.element.id) { index, column in
                if index > 0 {
                    Rectangle().fill(Palette.divider).frame(width: 1)
                }
                AccountColumn(column: column)
                    .padding(.horizontal, 14)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

@main
struct CcswapWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: "ccswap.usage", provider: Provider()) {
            CcswapWidgetView(entry: $0)
        }
        .configurationDisplayName("Usage limits")
        .description("Every account's usage, with a button to switch.")
        .supportedFamilies([.systemExtraLarge])
        .contentMarginsDisabled()
    }
}
