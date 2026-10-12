import Charts
import SwiftUI

struct Sparkline: View {
    let samples: [MetricSample]
    let value: (MetricSample) -> Double
    let color: Color
    var showAxis = false
    var format: (Double) -> String = { String(format: "%.0f", $0) }

    var body: some View {
        Chart {
            ForEach(samples, id: \.ts) { s in
                AreaMark(x: .value("Time", s.date), y: .value("Value", value(s)))
                    .interpolationMethod(.monotone)
                    .foregroundStyle(LinearGradient(colors: [color.opacity(0.35), color.opacity(0.02)], startPoint: .top, endPoint: .bottom))
                LineMark(x: .value("Time", s.date), y: .value("Value", value(s)))
                    .interpolationMethod(.monotone)
                    .foregroundStyle(color)
                    .lineStyle(StrokeStyle(lineWidth: 1.5, lineCap: .round))
            }
        }
        .chartXAxis(showAxis ? .automatic : .hidden)
        .chartYAxis {
            if showAxis {
                AxisMarks(position: .trailing, values: .automatic(desiredCount: 3)) { v in
                    AxisGridLine().foregroundStyle(.quaternary)
                    AxisValueLabel { if let d = v.as(Double.self) { Text(format(d)).font(.system(size: 9)) } }
                }
            }
        }
        .chartLegend(.hidden)
        .animation(.easeInOut(duration: 0.3), value: samples.count)
    }
}

struct MetricCard: View {
    let title: String
    let value: String
    let samples: [MetricSample]
    let pick: (MetricSample) -> Double
    let color: Color
    let format: (Double) -> String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(title).font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary).textCase(.uppercase)
                Spacer()
                Text(value).font(.system(size: 12, weight: .semibold)).monospacedDigit()
            }
            Sparkline(samples: samples, value: pick, color: color, showAxis: true, format: format)
                .frame(height: 54)
        }
        .padding(8)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(.background.opacity(0.5)))
    }
}
