import AppIntents
import SwiftUI
import WidgetKit

struct Entry: TimelineEntry {
    let date: Date
    let feed: Feed?
    /// false = server unreachable, showing the last good copy.
    let live: Bool
}

struct Provider: TimelineProvider {
    func placeholder(in context: Context) -> Entry { Entry(date: .now, feed: .sample, live: true) }

    func getSnapshot(in context: Context, completion: @escaping (Entry) -> Void) {
        if context.isPreview { return completion(placeholder(in: context)) }
        Task { let r = await FeedStore.load(); completion(Entry(date: .now, feed: r.feed ?? .sample, live: r.live)) }
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<Entry>) -> Void) {
        let next = Calendar.current.date(byAdding: .minute, value: 15, to: .now)!
        // Just poked: answer from the cache right away (no network), show the
        // reaction, and settle back a few seconds later.
        if let poke = Poke.recent(), let cached = FeedStore.cached() {
            let reacting = Entry(date: .now, feed: cached.poking(poke.id, face: poke.face), live: true)
            let settled = Entry(date: .now.addingTimeInterval(2.5), feed: cached, live: true)
            return completion(Timeline(entries: [reacting, settled], policy: .after(next)))
        }
        Task {
            let r = await FeedStore.load()
            // iOS rations widget refreshes; ask for 15 min, it may stretch that.
            completion(Timeline(entries: [Entry(date: .now, feed: r.feed, live: r.live)], policy: .after(next)))
        }
    }
}

// MARK: - Poke

/// Tap a ball on the home screen: it pulls a face for a moment.
struct PokeIntent: AppIntent {
    static var title: LocalizedStringResource = "戳一下"
    static var isDiscoverable = false

    @Parameter(title: "球") var ballId: String

    init() {}
    init(ballId: String) { self.ballId = ballId }

    func perform() async throws -> some IntentResult {
        Poke.record(ballId)
        return .result()
    }
}

enum Poke {
    private static let key = "dango.widget.poke"
    /// 害羞 / 惊讶 / 开心 / 好奇 — the same faces the capsule makes.
    private static let faces = ["14", "13", "10", "03"]

    static func record(_ id: String) {
        let last = UserDefaults.standard.dictionary(forKey: key)
        let n = ((last?["n"] as? Int) ?? 0) + 1
        UserDefaults.standard.set(["id": id, "at": Date.now.timeIntervalSince1970, "n": n], forKey: key)
    }

    static func recent() -> (id: String, face: String)? {
        guard let d = UserDefaults.standard.dictionary(forKey: key),
              let id = d["id"] as? String, let at = d["at"] as? Double,
              Date.now.timeIntervalSince1970 - at < 5 else { return nil }
        return (id, faces[((d["n"] as? Int) ?? 0) % faces.count])
    }
}

extension Feed {
    /// The same feed with one ball wearing a poke face (a crying ball stays crying).
    func poking(_ id: String, face: String) -> Feed {
        var copy = self
        if let i = copy.balls.firstIndex(where: { $0.id == id }), copy.balls[i].ok { copy.balls[i].emotion = face }
        return copy
    }
}

// MARK: - Views

struct DangoWidgetView: View {
    @Environment(\.widgetFamily) private var family
    let entry: Entry

    var body: some View {
        Group {
            if let feed = entry.feed, !feed.balls.isEmpty {
                switch family {
                case .systemSmall: SmallView(feed: feed)
                case .systemLarge: LargeView(feed: feed)
                case .accessoryCircular: CircularView(feed: feed)
                case .accessoryRectangular: RectangularView(feed: feed)
                default: MediumView(feed: feed)
                }
            } else {
                EmptyFeedView()
            }
        }
        .containerBackground(for: .widget) { GuavaBackground() }
    }
}

/// "3 分钟前" only when the Mac's numbers are old enough to matter.
struct AgeTag: View {
    let feed: Feed
    var body: some View {
        if Fmt.isStale(feed) {
            Text(Fmt.age(feed.fetchedDate)).font(.system(size: 10, weight: .medium)).foregroundStyle(.secondary)
        }
    }
}

struct BallCell: View {
    let ball: Feed.Ball
    var size: CGFloat
    var body: some View {
        Button(intent: PokeIntent(ballId: ball.id)) {
            VStack(spacing: 2) {
                BallSlot(ball: ball, lineWidth: 2.4)
                    .frame(width: size, height: size)
                    .id(ball.emotion)
                    .transition(.scale(scale: 0.7).combined(with: .opacity))
                Text(ball.ok ? Fmt.percent(ball.percent) : "!")
                    .font(.system(size: 11, weight: .semibold, design: .monospaced))
                    .foregroundStyle(ball.ok ? (ball.percent ?? 100) < 20 ? Color.dangoWarn : Color.primary : Color.dangoDanger)
                    .lineLimit(1)
            }
        }
        .buttonStyle(.plain)
    }
}

/// Balls in rows of `perRow`, sized so two rows always fit the family.
struct BallGrid: View {
    let balls: [Feed.Ball]
    let perRow: Int
    let size: CGFloat
    var body: some View {
        let rows = stride(from: 0, to: balls.count, by: perRow).map { Array(balls[$0..<min($0 + perRow, balls.count)]) }
        VStack(spacing: 6) {
            ForEach(rows.indices, id: \.self) { r in
                HStack(spacing: 0) {
                    ForEach(rows[r]) { ball in BallCell(ball: ball, size: size).frame(maxWidth: .infinity) }
                    // keep a short last row aligned with the columns above
                    ForEach(0..<(perRow - rows[r].count), id: \.self) { _ in Color.clear.frame(maxWidth: .infinity, maxHeight: 1) }
                }
            }
        }
    }
}

struct SmallView: View {
    let feed: Feed
    var body: some View {
        BallGrid(balls: Array(feed.balls.prefix(4)), perRow: 2, size: 46)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .overlay(alignment: .bottomTrailing) {
                HStack(spacing: 4) {
                    if feed.balls.count > 4 { Text("+\(feed.balls.count - 4)") }
                    AgeTag(feed: feed)
                }
                .font(.system(size: 9)).foregroundStyle(.secondary)
                .offset(x: 6, y: 8)
            }
    }
}

struct MediumView: View {
    let feed: Feed
    var body: some View {
        let balls = Array(feed.balls.prefix(8))
        BallGrid(balls: balls, perRow: balls.count <= 4 ? max(1, balls.count) : 4, size: 48)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .overlay(alignment: .bottomTrailing) { AgeTag(feed: feed).offset(x: 6, y: 8) }
    }
}

struct LargeView: View {
    let feed: Feed
    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack {
                Text("Dango").font(.system(size: 15, weight: .bold, design: .serif))
                Spacer()
                Text(Fmt.age(feed.fetchedDate)).font(.system(size: 10)).foregroundStyle(.secondary)
            }
            ForEach(feed.balls.prefix(7)) { ball in BallRow(ball: ball, compact: true) }
            Spacer(minLength: 0)
        }
    }
}

/// Lock screen: the most worrying ball as a gauge.
struct CircularView: View {
    let feed: Feed
    var body: some View {
        let ball = Feed.mostUrgent(feed.balls)
        Gauge(value: ball?.ok == true ? min(1, (ball?.percent ?? 0) / 100) : 0) {
            Text(ball?.name.prefix(2) ?? "")
        } currentValueLabel: {
            Text(ball?.ok == true ? Fmt.percent(ball?.percent) : "!")
        }
        .gaugeStyle(.accessoryCircular)
        .widgetLabel(ball?.name ?? "Dango")
    }
}

struct RectangularView: View {
    let feed: Feed
    var body: some View {
        let low = feed.balls.sorted { Feed.urgency($0) < Feed.urgency($1) }.prefix(3)
        VStack(alignment: .leading, spacing: 1) {
            ForEach(Array(low)) { ball in
                HStack {
                    Text(ball.name).lineLimit(1)
                    Spacer()
                    Text(ball.ok ? Fmt.percent(ball.percent) + "%" : "查不到").font(.system(.body, design: .monospaced))
                }
                .font(.system(size: 13, weight: .medium))
            }
        }
    }
}

struct EmptyFeedView: View {
    var body: some View {
        VStack(spacing: 6) {
            BallFace(shape: "blob", emotion: "02", color: "#D8A7A0").frame(width: 40, height: 40)
            Text("还没收到 Mac 的数据").font(.system(size: 11)).foregroundStyle(.secondary).multilineTextAlignment(.center)
        }
    }
}

extension Feed {
    /// Lower = more urgent. Errors first, then by remaining percent.
    static func urgency(_ ball: Ball) -> Double { ball.ok ? (ball.percent ?? 200) : -1 }
    static func mostUrgent(_ balls: [Ball]) -> Ball? { balls.min { urgency($0) < urgency($1) } }

    static let sample = Feed(v: 1, fetchedAt: Date.now.timeIntervalSince1970, balls: [
        Ball(id: "claude", name: "Claude", color: "#E7A97C", shape: "star", emotion: "10", ok: true, percent: 81, label: "7 天窗口", hint: nil, resetsAt: nil, buckets: []),
        Ball(id: "cursor", name: "Cursor", color: "#C9BCA6", shape: "blob", emotion: "12", ok: true, percent: 11, label: "本月", hint: nil, resetsAt: nil, buckets: []),
        Ball(id: "devin", name: "Devin", color: "#A8C6A2", shape: "wedge", emotion: "10", ok: true, percent: 100, label: "每日", hint: nil, resetsAt: nil, buckets: []),
        Ball(id: "deepseek", name: "DeepSeek", color: "#7FA8D6", shape: "whale", emotion: "19", ok: true, percent: 46, label: "余额", hint: nil, resetsAt: nil, buckets: []),
    ])
}

@main
struct DangoWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: "DangoWidget", provider: Provider()) { entry in
            DangoWidgetView(entry: entry)
        }
        .configurationDisplayName("额度团子")
        .description("每颗球是一家 AI 套餐的余量，数据来自你的 Mac。")
        .supportedFamilies([.systemSmall, .systemMedium, .systemLarge, .accessoryCircular, .accessoryRectangular])
    }
}
