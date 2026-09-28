import SwiftUI
import UIKit
import WebKit
import WidgetKit

@main
struct DangoApp: App {
    var body: some Scene {
        WindowGroup { ContentView() }
    }
}

/// Feed + the one-line reason it isn't live, shared by the web capsule.
@MainActor
final class DangoModel: ObservableObject {
    @Published private(set) var feed: Feed? = FeedStore.cached()
    @Published private(set) var problem: String?
    private var loading = false

    func reload() async {
        guard !loading else { return }
        loading = true
        let r = await FeedStore.fetch()
        if let f = r.feed { feed = f }
        problem = r.error
        loading = false
        WidgetCenter.shared.reloadAllTimelines()
    }
}

struct ContentView: View {
    @StateObject private var model = DangoModel()
    @Environment(\.scenePhase) private var phase
    private let tick = Timer.publish(every: 60, on: .main, in: .common).autoconnect()

    var body: some View {
        CapsuleWeb(model: model)
            .ignoresSafeArea()
            .task { await model.reload() }
            .onChange(of: phase) { _, now in if now == .active { Task { await model.reload() } } }
            .onReceive(tick) { _ in if phase == .active { Task { await model.reload() } } }
    }
}

/// The same grok-ball engine as the desktop widget and the product page,
/// running in a web view so balls blink, follow your finger and react to pokes.
struct CapsuleWeb: UIViewRepresentable {
    @ObservedObject var model: DangoModel

    func makeCoordinator() -> Coordinator { Coordinator(model: model) }

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        for name in Coordinator.messages { config.userContentController.add(context.coordinator, name: name) }
        let web = WKWebView(frame: .zero, configuration: config)
        web.isOpaque = false
        web.backgroundColor = .clear
        web.scrollView.isScrollEnabled = false
        web.scrollView.bounces = false
        web.scrollView.contentInsetAdjustmentBehavior = .never
        context.coordinator.web = web
        if let page = Bundle.main.url(forResource: "capsule", withExtension: "html") {
            web.loadFileURL(page, allowingReadAccessTo: page.deletingLastPathComponent())
        }
        return web
    }

    func updateUIView(_ web: WKWebView, context: Context) {
        context.coordinator.push()
    }

    final class Coordinator: NSObject, WKScriptMessageHandler {
        static let messages = ["ready", "refresh", "poke", "tap"]
        let model: DangoModel
        weak var web: WKWebView?
        private var ready = false
        private let soft = UIImpactFeedbackGenerator(style: .soft)
        private let rigid = UIImpactFeedbackGenerator(style: .rigid)
        private let light = UIImpactFeedbackGenerator(style: .light)

        init(model: DangoModel) { self.model = model }

        @MainActor func push() {
            guard ready, let web else { return }
            let feedJSON = model.feed.flatMap { try? JSONEncoder().encode($0) }.flatMap { String(data: $0, encoding: .utf8) } ?? "null"
            let meta: [String: Any] = ["live": model.problem == nil, "problem": model.problem ?? NSNull()]
            let metaJSON = (try? JSONSerialization.data(withJSONObject: meta)).flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
            web.evaluateJavaScript("window.dango && window.dango.setFeed(\(feedJSON), \(metaJSON))")
        }

        func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            Task { @MainActor in
                switch message.name {
                case "ready":
                    ready = true
                    push()
                case "refresh":
                    await model.reload()
                    push()
                case "poke":
                    switch message.body as? String {
                    case "party": UINotificationFeedbackGenerator().notificationOccurred(.success)
                    case "dizzy": rigid.impactOccurred()
                    default: soft.impactOccurred()
                    }
                default:
                    light.impactOccurred(intensity: 0.6)
                }
            }
        }
    }
}
