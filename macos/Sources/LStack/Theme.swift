import SwiftUI

enum Theme {
    static func statusColor(_ status: String) -> Color {
        switch status {
        case "running": return .green
        case "starting", "stopping": return .orange
        case "crashed", "errored": return .red
        default: return .secondary.opacity(0.6)
        }
    }

    static func typeIcon(_ type: String) -> String {
        switch type {
        case "process": return "terminal.fill"
        case "docker": return "shippingbox.fill"
        case "compose": return "square.stack.3d.up.fill"
        case "k8s": return "helm"
        default: return "questionmark.circle"
        }
    }

    static func typeColor(_ type: String) -> Color {
        switch type {
        case "process": return .indigo
        case "docker": return .blue
        case "compose": return .cyan
        case "k8s": return .purple
        default: return .gray
        }
    }
}

struct StatusDot: View {
    let status: String
    var size: CGFloat = 8

    var body: some View {
        let color = Theme.statusColor(status)
        Circle()
            .fill(color)
            .frame(width: size, height: size)
            .shadow(color: status == "running" ? color.opacity(0.8) : .clear, radius: 3)
            .overlay {
                // Only transitional states pulse. A repeating animation redraws every frame for as long
                // as it exists (even invisible, even in the closed menu bar panel), so don't create it otherwise.
                if status == "starting" || status == "stopping" { PulseRing(color: color) }
            }
    }
}

private struct PulseRing: View {
    let color: Color
    @State private var pulse = false

    var body: some View {
        Circle()
            .stroke(color.opacity(0.5), lineWidth: 2)
            .scaleEffect(pulse ? 2.2 : 1)
            .opacity(pulse ? 0 : 1)
            .onAppear {
                withAnimation(.easeOut(duration: 1.2).repeatForever(autoreverses: false)) { pulse = true }
            }
    }
}

struct TypeBadge: View {
    let type: String
    var body: some View {
        Image(systemName: Theme.typeIcon(type))
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: 22, height: 22)
            .background(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .fill(Theme.typeColor(type).gradient)
            )
    }
}

struct Pill: View {
    let icon: String
    let text: String
    var tint: Color = .secondary
    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: icon).font(.system(size: 9, weight: .bold))
            Text(text).font(.system(size: 11, weight: .medium)).monospacedDigit()
        }
        .foregroundStyle(tint)
        .padding(.horizontal, 7)
        .padding(.vertical, 3)
        .background(Capsule().fill(tint.opacity(0.12)))
    }
}

struct IconButton: View {
    let icon: String
    let help: String
    var tint: Color = .primary
    let action: () -> Void
    @State private var hover = false

    var body: some View {
        Button(action: action) {
            Image(systemName: icon)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(tint)
                .frame(width: 26, height: 24)
                .background(RoundedRectangle(cornerRadius: 6, style: .continuous).fill(hover ? Color.primary.opacity(0.1) : .clear))
        }
        .buttonStyle(.plain)
        .help(help)
        .onHover { hover = $0 }
    }
}

/// Lays children out left to right, wrapping onto new lines when the row is full (chips, tags).
struct FlowLayout: Layout {
    var spacing: CGFloat = 4
    var lineSpacing: CGFloat = 4

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let maxW = proposal.width ?? .infinity
        var x: CGFloat = 0, y: CGFloat = 0, lineH: CGFloat = 0, width: CGFloat = 0
        for s in subviews {
            let sz = s.sizeThatFits(.unspecified)
            let w = min(sz.width, maxW)
            if x > 0, x + w > maxW {
                y += lineH + lineSpacing
                x = 0
                lineH = 0
            }
            x += w + spacing
            lineH = max(lineH, sz.height)
            width = max(width, x - spacing)
        }
        return CGSize(width: width, height: y + lineH)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var x = bounds.minX, y = bounds.minY, lineH: CGFloat = 0
        for s in subviews {
            let sz = s.sizeThatFits(.unspecified)
            let w = min(sz.width, bounds.width)
            if x > bounds.minX, x + w > bounds.maxX {
                y += lineH + lineSpacing
                x = bounds.minX
                lineH = 0
            }
            s.place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(width: w, height: sz.height))
            x += w + spacing
            lineH = max(lineH, sz.height)
        }
    }
}
