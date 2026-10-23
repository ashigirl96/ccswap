import AppIntents
import Foundation

/// The widget is sandboxed and cannot switch accounts itself; the menu bar
/// picks this request up within a second.
struct SwitchAccountIntent: AppIntent {
    static let title: LocalizedStringResource = "Switch account"
    static let isDiscoverable = false

    @Parameter(title: "Account number") var number: String
    @Parameter(title: "Provider", default: "claude") var provider: String

    init() {}
    init(number: String, provider: Feed.ProviderKind) {
        self.number = number
        self.provider = provider.rawValue
    }

    /// The reload that follows an intent is guaranteed, unlike timeline
    /// reloads, so it waits for the menu bar's post-switch feed to show it.
    func perform() async throws -> some IntentResult {
        let kind = Feed.ProviderKind(rawValue: provider) ?? .claude
        let requestedAt = Date().timeIntervalSince1970
        let body: [String: Any] = ["switch_to": number, "provider": kind.rawValue, "at": requestedAt]
        let data = try JSONSerialization.data(withJSONObject: body)
        try data.write(to: SharedFiles.request, options: .atomic)
        let deadline = Date.now.addingTimeInterval(5)
        while Date.now < deadline {
            if let feed = SharedFiles.loadFeed(), feed.writtenAt > requestedAt,
               feed.accounts(of: kind).contains(where: { $0.number == number && $0.active }) {
                break
            }
            try await Task.sleep(for: .milliseconds(200))
        }
        return .result()
    }
}
