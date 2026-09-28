import SwiftUI

extension Color {
    init(hex: String) {
        let (r, g, b) = Color.rgb(hex)
        self.init(.sRGB, red: r, green: g, blue: b)
    }

    static func rgb(_ hex: String) -> (Double, Double, Double) {
        let h = hex.hasPrefix("#") ? String(hex.dropFirst()) : hex
        guard h.count == 6, let v = UInt32(h, radix: 16) else { return (0.72, 0.69, 0.64) }
        return (Double((v >> 16) & 0xFF) / 255, Double((v >> 8) & 0xFF) / 255, Double(v & 0xFF) / 255)
    }

    /// grok-ball `shade`: mix toward white (amt > 0) or black (amt < 0).
    static func shade(_ hex: String, _ amt: Double) -> Color {
        let (r, g, b) = rgb(hex)
        let t = amt < 0 ? 0.0 : 1.0, a = abs(amt)
        return Color(.sRGB, red: r + (t - r) * a, green: g + (t - g) * a, blue: b + (t - b) * a)
    }

    static let dangoWarn = Color(hex: "#C0791A")
    static let dangoDanger = Color(hex: "#C74A3F")
    static let dangoEye = Color(hex: "#F5F2ED")
}

/// One grok-ball face, drawn from the frames exported by
/// `cargo run -p grok-ball --example frame_dump`.
struct BallFace: View {
    var shape: String
    var emotion: String
    var color: String

    private var frame: BallFrame? {
        BallShapes.frames["\(shape)/\(emotion)"] ?? BallShapes.frames["blob/\(emotion)"] ?? BallShapes.frames["blob/02"]
    }

    var body: some View {
        GeometryReader { geo in
            let size = min(geo.size.width, geo.size.height)
            if let frame {
                ZStack {
                    BallPath(points: frame.head)
                        .fill(RadialGradient(
                            stops: headStops(frame),
                            center: UnitPoint(x: 0.38, y: 0.32),
                            startRadius: 0,
                            endRadius: size * 0.75))
                    ForEach(frame.eyes.indices, id: \.self) { i in
                        BallPath(points: frame.eyes[i]).fill(Color.dangoEye)
                    }
                }
                .frame(width: size, height: size)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .aspectRatio(1, contentMode: .fit)
    }

    private func headStops(_ frame: BallFrame) -> [Gradient.Stop] {
        if let fixed = frame.headColors, fixed.count == 3 {
            return zip([0, 0.62, 1], fixed).map { Gradient.Stop(color: Color(hex: $1), location: $0) }
        }
        return [
            .init(color: .shade(color, 0.22), location: 0),
            .init(color: Color(hex: color), location: 0.62),
            .init(color: .shade(color, -0.12), location: 1),
        ]
    }
}

/// A closed polygon in grok-ball's viewBox (-15 -15 259 259), scaled to the rect.
struct BallPath: Shape {
    var points: [Double]

    func path(in rect: CGRect) -> Path {
        var path = Path()
        let s = min(rect.width, rect.height) / 259
        let ox = rect.midX - 259 * s / 2, oy = rect.midY - 259 * s / 2
        var i = 0
        while i + 1 < points.count {
            let p = CGPoint(x: ox + (points[i] + 15) * s, y: oy + (points[i + 1] + 15) * s)
            if i == 0 { path.move(to: p) } else { path.addLine(to: p) }
            i += 2
        }
        path.closeSubpath()
        return path
    }
}

/// Ball + remaining-quota ring, the capsule's slot on a phone.
struct BallSlot: View {
    var ball: Feed.Ball
    var lineWidth: CGFloat = 3

    /// theme.rs: >=50 plan colour (faded), 20–50 plan colour, >0 orange, 0 red.
    private var ringColor: Color {
        guard ball.ok, let p = ball.percent else { return .dangoDanger }
        if p <= 0 { return .dangoDanger }
        if p < 20 { return .dangoWarn }
        return Color(hex: ball.color).opacity(p >= 50 ? 0.55 : 1)
    }

    var body: some View {
        ZStack {
            Circle().stroke(Color.primary.opacity(0.12), lineWidth: lineWidth)
            if !ball.ok {
                Circle().stroke(Color.dangoDanger, style: StrokeStyle(lineWidth: lineWidth, lineCap: .round, dash: [lineWidth * 1.2, lineWidth * 1.8]))
            } else if let p = ball.percent {
                Circle()
                    .trim(from: 0, to: max(0.001, min(1, p / 100)))
                    .stroke(ringColor, style: StrokeStyle(lineWidth: lineWidth, lineCap: .round))
                    .rotationEffect(.degrees(-90))
            }
            BallFace(shape: ball.shape, emotion: ball.emotion, color: ball.color)
                .padding(lineWidth * 2.2)
        }
        .aspectRatio(1, contentMode: .fit)
    }
}

/// Bar for one quota window: plan colour, orange when low, red when empty.
struct QuotaBar: View {
    var percent: Double?
    var color: String

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.primary.opacity(0.1))
                if let p = percent {
                    Capsule()
                        .fill(p <= 0 ? Color.dangoDanger : p < 20 ? Color.dangoWarn : Color(hex: color))
                        .frame(width: max(geo.size.height, geo.size.width * min(1, p / 100)))
                        .opacity(p <= 0 ? 0 : 1)
                }
            }
        }
    }
}

/// 芭乐底光：粉瓤左上、青皮右下。
struct GuavaBackground: View {
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        let dark = scheme == .dark
        ZStack {
            LinearGradient(colors: dark
                ? [Color(hex: "#231A18"), Color(hex: "#1C1817"), Color(hex: "#161C16")]
                : [Color(hex: "#F3E8E6"), Color(hex: "#EEE7E4"), Color(hex: "#E3EAE2")],
                startPoint: .topLeading, endPoint: .bottomTrailing)
            RadialGradient(colors: [Color(hex: dark ? "#3A2C2B" : "#EBD9D7").opacity(0.9), .clear],
                           center: UnitPoint(x: 0.1, y: 0.05), startRadius: 0, endRadius: 220)
            RadialGradient(colors: [Color(hex: dark ? "#28332A" : "#D3DFD1").opacity(0.9), .clear],
                           center: UnitPoint(x: 0.95, y: 0.95), startRadius: 0, endRadius: 240)
        }
    }
}
