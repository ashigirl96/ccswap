import Foundation

struct Feed: Decodable {
    let v: Int
    let writtenAt: Double
    let staleAfter: Double
    let threshold: Int
    let accounts: [Account]

    struct Account: Decodable, Identifiable {
        let number: String
        let label: String
        let active: Bool
        let disabled: Bool
        let note: String?
        let windows: [Window]

        var id: String { number }
    }

    struct Window: Decodable, Identifiable {
        let name: String
        let pct: Double
        let resetsAt: Double?

        var id: String { name }
        var resetDate: Date? { resetsAt.map(Date.init(timeIntervalSince1970:)) }
    }

    var writtenDate: Date { Date(timeIntervalSince1970: writtenAt) }
    var staleDate: Date { Date(timeIntervalSince1970: writtenAt + staleAfter) }

    static let supportedVersion = 1
}

enum SharedFiles {
    /// The sandbox points NSHomeDirectory() at the container; the entitlement
    /// exception names a path under the real home.
    static var directory: URL {
        let home = String(cString: getpwuid(getuid())!.pointee.pw_dir)
        return URL(fileURLWithPath: home).appendingPathComponent(".claude-swap-backup/widget")
    }

    static var feed: URL { directory.appendingPathComponent("feed.json") }
    static var request: URL { directory.appendingPathComponent("request.json") }

    static func loadFeed() -> Feed? {
        guard let data = try? Data(contentsOf: feed) else { return nil }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        guard let feed = try? decoder.decode(Feed.self, from: data),
              feed.v == Feed.supportedVersion else { return nil }
        return feed
    }
}
