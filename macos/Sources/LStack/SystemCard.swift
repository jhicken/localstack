import Charts
import SwiftUI

/// Whole-machine CPU and memory, split into LStack services vs. everything else on the Mac.
struct SystemCard: View {
    let info: SystemInfo

    var body: some View {
        let last = info.samples.last
        HStack(spacing: 6) {
            LoadTile(
                title: "CPU",
                value: Fmt.pct(last?.cpu),
                detail: "\(info.cores) cores",
                samples: info.samples,
                total: \.cpu,
                ours: \.lsCpu,
                capacity: 100,
                color: .green,
                format: { Fmt.pct($0) }
            )
            LoadTile(
                title: "Memory",
                value: last.map { "\(Fmt.gb($0.mem)) / \(Fmt.gb(info.memTotal))" } ?? "—",
                detail: nil,
                samples: info.samples,
                total: \.mem,
                ours: \.lsMem,
                capacity: info.memTotal,
                color: .blue,
                format: { Fmt.bytes($0) }
            )
        }
    }
}

private struct LoadTile: View {
    let title: String
    let value: String
    let detail: String?
    let samples: [SystemSample]
    let total: KeyPath<SystemSample, Double>
    let ours: KeyPath<SystemSample, Double>
    let capacity: Double
    let color: Color
    let format: (Double) -> String

    private var last: SystemSample? { samples.last }
    private var load: Double { guard let last, capacity > 0 else { return 0 } ; return last[keyPath: total] / capacity }
    private var level: Color { load >= 0.9 ? .red : load >= 0.75 ? .orange : .primary }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(alignment: .firstTextBaseline) {
                Text(title.uppercased()).font(.system(size: 9, weight: .bold)).foregroundStyle(.secondary)
                Spacer(minLength: 4)
                Text(value).font(.system(size: 12, weight: .semibold)).monospacedDigit().foregroundStyle(level)
            }
            chart.frame(height: 30)
            bar.frame(height: 5)
            HStack(spacing: 8) {
                legend(color, "LStack", last.map { format($0[keyPath: ours]) } ?? "—")
                legend(.gray, "Other", last.map { format(max($0[keyPath: total] - $0[keyPath: ours], 0)) } ?? "—")
            }
            .lineLimit(1)
        }
        .padding(8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Color.primary.opacity(0.05)))
        .help(title == "CPU" ? "Share of all \(detail ?? "") on this Mac. Per-service CPU below is % of one core." : "Memory in use (app + wired + compressed), as Activity Monitor reports it.")
    }

    /// CPU zooms to recent peaks so light load is still readable; memory is shown against total RAM.
    private var yMax: Double {
        guard title == "CPU" else { return max(capacity, 1) }
        let peak = samples.map { $0[keyPath: total] }.max() ?? 0
        return min(max(peak * 1.25, 10), 100)
    }

    private var chart: some View {
        Chart {
            ForEach(samples, id: \.ts) { s in
                AreaMark(x: .value("Time", s.date), y: .value("All", s[keyPath: total]), series: .value("Series", "All"))
                    .interpolationMethod(.monotone)
                    .foregroundStyle(Color.gray.opacity(0.25))
                AreaMark(x: .value("Time", s.date), y: .value("LStack", s[keyPath: ours]), series: .value("Series", "LStack"))
                    .interpolationMethod(.monotone)
                    .foregroundStyle(color.opacity(0.45))
                LineMark(x: .value("Time", s.date), y: .value("All", s[keyPath: total]), series: .value("Series", "AllLine"))
                    .interpolationMethod(.monotone)
                    .foregroundStyle(Color.gray.opacity(0.8))
                    .lineStyle(StrokeStyle(lineWidth: 1, lineCap: .round))
            }
        }
        .chartXAxis(.hidden)
        .chartYAxis(.hidden)
        .chartYScale(domain: 0...yMax)
        .chartLegend(.hidden)
    }

    /// LStack | other apps | free, as a share of capacity.
    private var bar: some View {
        GeometryReader { g in
            let w = g.size.width
            let t = min(max((last?[keyPath: total] ?? 0) / max(capacity, 1), 0), 1)
            let o = min(max((last?[keyPath: ours] ?? 0) / max(capacity, 1), 0), t)
            ZStack(alignment: .leading) {
                Capsule().fill(Color.primary.opacity(0.08))
                Capsule().fill(Color.gray.opacity(0.6)).frame(width: w * t)
                Capsule().fill(color).frame(width: w * o)
            }
        }
    }

    private func legend(_ c: Color, _ label: String, _ v: String) -> some View {
        HStack(spacing: 3) {
            Circle().fill(c).frame(width: 5, height: 5)
            Text(label).font(.system(size: 9)).foregroundStyle(.secondary)
            Text(v).font(.system(size: 9, weight: .medium)).monospacedDigit()
        }
    }
}

/// Keeps the menu bar window's height equal to its content. SwiftUI grows a
/// `MenuBarExtra(.window)` panel when content gets taller but never shrinks it.
struct MenuWindowSizer: NSViewRepresentable {
    let height: CGFloat

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        let h = height
        DispatchQueue.main.async {
            guard h > 0, let w = view.window else { return }
            let target = w.frameRect(forContentRect: NSRect(x: 0, y: 0, width: w.contentLayoutRect.width, height: h)).height
            var f = w.frame
            guard abs(f.height - target) > 0.5 else { return }
            f.origin.y += f.height - target  // keep the top edge pinned under the menu bar
            f.size.height = target
            w.setFrame(f, display: true)
        }
    }
}
