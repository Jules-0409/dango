import SwiftUI

/// Name, headline number, main bar and the next reset — shared with the app.
struct BallRow: View {
    let ball: Feed.Ball
    var compact = false

    var body: some View {
        HStack(spacing: 10) {
            BallSlot(ball: ball, lineWidth: compact ? 2.2 : 3)
                .frame(width: compact ? 34 : 46, height: compact ? 34 : 46)
            VStack(alignment: .leading, spacing: 3) {
                HStack(alignment: .firstTextBaseline) {
                    Text(ball.name).font(.system(size: compact ? 13 : 15, weight: .semibold)).lineLimit(1)
                    Spacer(minLength: 4)
                    if ball.ok {
                        Text(Fmt.percent(ball.percent) + (ball.percent == nil ? "" : "%"))
                            .font(.system(size: compact ? 13 : 16, weight: .semibold, design: .monospaced))
                    }
                }
                if ball.ok {
                    QuotaBar(percent: ball.percent, color: ball.color).frame(height: compact ? 4 : 5)
                    HStack {
                        Text(ball.label ?? "").lineLimit(1)
                        Spacer(minLength: 4)
                        if let reset = ball.nextReset { Text(Fmt.until(reset) + "后重置") }
                    }
                    .font(.system(size: compact ? 10 : 11)).foregroundStyle(.secondary)
                } else {
                    Text(ball.hint ?? "查不到").font(.system(size: compact ? 11 : 12)).foregroundStyle(Color.dangoDanger).lineLimit(2)
                }
            }
        }
    }
}
