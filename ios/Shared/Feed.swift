import Foundation

/// What the Mac publishes (`GET 127.0.0.1:8049/feed`, pushed by scripts/push-feed.sh).
struct Feed: Codable, Equatable {
    var v: Int
    /// Unix seconds.
    var fetchedAt: Double
    var balls: [Ball]

    struct Ball: Codable, Equatable, Identifiable {
        var id: String
        var name: String
        var color: String
        var shape: String
        var emotion: String
        var ok: Bool
        var percent: Double?
        var label: String?
        var hint: String?
        /// Unix ms.
        var resetsAt: Double?
        var buckets: [Bucket]
    }

    struct Bucket: Codable, Equatable {
        var label: String
        var percent: Double?
        var resetsAt: Double?
    }

    var fetchedDate: Date { Date(timeIntervalSince1970: fetchedAt) }
}

extension Feed.Ball {
    /// Earliest reset among the headline and its buckets that is still ahead.
    var nextReset: Date? {
        let all = ([resetsAt] + buckets.map(\.resetsAt)).compactMap { $0 }
        return all.map { Date(timeIntervalSince1970: $0 / 1000) }.filter { $0 > .now }.min()
    }
}

enum FeedStore {
    private static let cacheKey = "dango.feed.cache"

    /// Fresh from the server; falls back to the last good copy (and says so).
    static func load() async -> (feed: Feed?, live: Bool) {
        let r = await fetch()
        return (r.feed, r.error == nil)
    }

    /// Like `load`, plus why the server copy wasn't used — for the app to show.
    static func fetch() async -> (feed: Feed?, error: String?) {
        var request = URLRequest(url: FeedConfig.url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 12)
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        let problem: String
        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            if status == 200 {
                do {
                    let feed = try JSONDecoder().decode(Feed.self, from: data)
                    UserDefaults.standard.set(data, forKey: cacheKey)
                    return (feed, nil)
                } catch {
                    return (cached(), "数据格式对不上：\(error.localizedDescription)")
                }
            }
            problem = status == 404 ? "服务器上还没有这份数据（404），Mac 那边的推送可能没在跑" : "服务器返回 \(status)"
        } catch let e as URLError where e.code == .notConnectedToInternet || e.code == .dataNotAllowed {
            problem = "连不上网。去「设置 → App → Dango → 无线数据」选「WLAN 与蜂窝网络」"
        } catch let e as URLError where e.code == .timedOut {
            problem = "连服务器超时了"
        } catch {
            problem = "请求失败：\(error.localizedDescription)"
        }
        return (cached(), problem)
    }

    static func cached() -> Feed? {
        guard let data = UserDefaults.standard.data(forKey: cacheKey) else { return nil }
        return try? JSONDecoder().decode(Feed.self, from: data)
    }
}

enum Fmt {
    static func percent(_ value: Double?) -> String {
        guard let value else { return "—" }
        return "\(Int(value.rounded()))"
    }

    /// "3 分钟前" — how old the Mac's numbers are.
    static func age(_ date: Date, now: Date = .now) -> String {
        let seconds = max(0, now.timeIntervalSince(date))
        if seconds < 90 { return "刚刚" }
        if seconds < 3600 { return "\(Int(seconds / 60)) 分钟前" }
        if seconds < 86400 { return "\(Int(seconds / 3600)) 小时前" }
        return "\(Int(seconds / 86400)) 天前"
    }

    /// "2 小时后重置" style countdown.
    static func until(_ date: Date, now: Date = .now) -> String {
        let seconds = max(0, date.timeIntervalSince(now))
        if seconds < 3600 { return "\(max(1, Int(seconds / 60))) 分钟" }
        if seconds < 86400 { return "\(Int(seconds / 3600)) 小时" }
        return "\(Int(seconds / 86400)) 天"
    }

    /// The Mac pushes every minute; older than this means it's asleep or offline.
    static func isStale(_ feed: Feed, now: Date = .now) -> Bool {
        now.timeIntervalSince(feed.fetchedDate) > 15 * 60
    }
}
