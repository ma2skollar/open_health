import Foundation
import SwiftUI

/// Hourly heart-rate candles — the day-shaped HR view behind the dashboard's heart
/// rate cell.
///
/// The nightly RHR trend says how the last few weeks of sleep went; it says nothing
/// about the day you are in. One candle per local-clock hour does: the wick is the
/// lowest and highest beat measured in that hour, the body runs from the first beat
/// to the last. Aggregation happens in Rust (`oura-summary::hourly_hr`) because it
/// walks the whole event table — the same read the summary does, not a second
/// decoding of the database in Swift.
enum HourlyHR {
    struct Candle: Identifiable {
        let unix: Double        // UTC start of the local hour
        let ymd: String
        let hour: Int
        let low: Double
        let high: Double
        let open: Double
        let close: Double
        let mean: Double
        let count: Int
        var id: Double { unix }
        var rising: Bool { close >= open }
    }

    struct Reading {
        let bpm: Double
        let unix: Double
    }

    struct Result {
        var candles: [Candle] = []
        var latest: Reading?
        var error: String?
    }

    /// Whole hours from UTC — the offset the whole stack (web `--tz-offset`, the model
    /// runners, this FFI) speaks. Matches `Core.base()`.
    static var tzOffset: Int64 {
        Int64((Double(TimeZone.current.secondsFromGMT()) / 3600).rounded())
    }

    static func load(days: UInt32) -> Result {
        let json = hourlyHrJson(dbPath: DB.readPath(), tzOffset: tzOffset, days: days)
        guard let data = json.data(using: .utf8),
              let root = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else { return Result(error: "unreadable response") }
        if let err = root["error"] as? String { return Result(error: err) }

        let candles = (root["hours"] as? [[String: Any]] ?? []).compactMap { row -> Candle? in
            guard let unix = (row["unix"] as? NSNumber)?.doubleValue,
                  let low = (row["low"] as? NSNumber)?.doubleValue,
                  let high = (row["high"] as? NSNumber)?.doubleValue,
                  let open = (row["open"] as? NSNumber)?.doubleValue,
                  let close = (row["close"] as? NSNumber)?.doubleValue
            else { return nil }
            return Candle(unix: unix,
                          ymd: row["ymd"] as? String ?? "",
                          hour: (row["hour"] as? NSNumber)?.intValue ?? 0,
                          low: low, high: high, open: open, close: close,
                          mean: (row["mean"] as? NSNumber)?.doubleValue ?? (low + high) / 2,
                          count: (row["count"] as? NSNumber)?.intValue ?? 0)
        }
        let latest = (root["latest"] as? [String: Any]).flatMap { l -> Reading? in
            guard let bpm = (l["bpm"] as? NSNumber)?.doubleValue,
                  let unix = (l["unix"] as? NSNumber)?.doubleValue else { return nil }
            return Reading(bpm: bpm, unix: unix)
        }
        return Result(candles: candles, latest: latest)
    }
}

enum HourlyWindow: String, CaseIterable {
    case h24 = "24h", h48 = "48h", d7 = "7d"
    /// Hour slots drawn, gaps included — the axis is real time, so a night the ring
    /// spent on the charger reads as a gap, not as a shorter chart.
    var hours: Int {
        switch self {
        case .h24: return 24
        case .h48: return 48
        case .d7: return 24 * 7
        }
    }
    /// What to ask the FFI for — one extra day so switching windows never refetches.
    var days: UInt32 { self == .d7 ? 8 : 2 }
}

/// The hourly candle panel: current reading, the candles, and the window's extremes.
struct HourlyHeartRateSection: View {
    @State private var window: HourlyWindow = .h24
    @State private var result = HourlyHR.Result()
    @State private var loading = true
    @State private var selected: HourlyHR.Candle?

    /// The candles inside the chosen window, anchored on the newest hour that has
    /// data — a ring that last synced yesterday still fills the chart.
    private var visible: [HourlyHR.Candle] {
        guard let newest = result.candles.last?.unix else { return [] }
        let cut = newest - Double((window.hours - 1) * 3600)
        return result.candles.filter { $0.unix >= cut }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                ObsTag("hourly", icon: "chart.bar.fill")
                Spacer()
                if loading { ProgressView().controlSize(.mini).tint(Obs.ink) }
            }
            Picker("Window", selection: $window) {
                ForEach(HourlyWindow.allCases, id: \.self) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented)
            .onChange(of: window) { _, _ in selected = nil; reload() }

            if let error = result.error {
                Text(error).font(Obs.mono(12)).foregroundStyle(Obs.bad)
                    .fixedSize(horizontal: false, vertical: true)
            } else if visible.isEmpty {
                Text(loading ? "Reading the ring database…"
                             : "No beats stored for this window yet. Sync the ring.")
                    .font(Obs.mono(12)).foregroundStyle(Obs.ink2)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                readout
                CandleChart(candles: visible, window: window, selected: $selected)
                    .frame(height: 210)
                axis
                stats
            }
        }
        .obsCard()
        .task { reload() }
    }

    /// Either the hour you are touching, or the latest reading when nothing is held.
    @ViewBuilder private var readout: some View {
        if let c = selected {
            VStack(alignment: .leading, spacing: 4) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text("\(Int(c.low.rounded()))–\(Int(c.high.rounded()))")
                        .font(Obs.mono(30, .medium)).foregroundStyle(Obs.ink).monospacedDigit()
                    Text("bpm").font(Obs.mono(13)).foregroundStyle(Obs.ink2)
                }
                Text("\(Self.dayLabel(c)) · \(String(format: "%02d:00", c.hour)) · open \(Int(c.open.rounded())) → close \(Int(c.close.rounded())) · \(c.count) beats")
                    .font(Obs.mono(10)).foregroundStyle(Obs.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
        } else if let latest = result.latest {
            VStack(alignment: .leading, spacing: 4) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text("\(Int(latest.bpm.rounded()))")
                        .font(Obs.mono(30, .medium)).foregroundStyle(Obs.ink).monospacedDigit()
                    Text("bpm").font(Obs.mono(13)).foregroundStyle(Obs.ink2)
                    Text("latest").font(Obs.mono(10)).foregroundStyle(Obs.muted)
                }
                Text(Self.stamp(latest.unix)).font(Obs.mono(10)).foregroundStyle(Obs.muted)
            }
        }
    }

    private var axis: some View {
        HStack {
            if let first = visible.first {
                Text(Self.dayLabel(first) + " " + String(format: "%02d:00", first.hour))
                    .font(Obs.mono(9)).foregroundStyle(Obs.ink2)
            }
            Spacer()
            Text("touch a bar for that hour").font(Obs.mono(9)).foregroundStyle(Obs.muted)
            Spacer()
            if let last = visible.last {
                Text(Self.dayLabel(last) + " " + String(format: "%02d:00", last.hour))
                    .font(Obs.mono(9)).foregroundStyle(Obs.ink2)
            }
        }
    }

    private var stats: some View {
        let lows = visible.map(\.low), highs = visible.map(\.high)
        let beats = visible.reduce(0) { $0 + $1.count }
        let weighted = visible.reduce(0.0) { $0 + $1.mean * Double($1.count) }
        return VStack(spacing: 10) {
            if let lo = lows.min() { ObsStat(label: "lowest", value: "\(Int(lo.rounded())) bpm") }
            if let hi = highs.max() { ObsStat(label: "highest", value: "\(Int(hi.rounded())) bpm") }
            if beats > 0 { ObsStat(label: "average", value: "\(Int((weighted / Double(beats)).rounded())) bpm") }
            ObsStat(label: "hours with data", value: "\(visible.count)/\(window.hours)")
        }
        .padding(.top, 2)
    }

    private func reload() {
        loading = true
        let days = window.days
        Task.detached(priority: .userInitiated) {
            let out = HourlyHR.load(days: days)
            await MainActor.run { result = out; loading = false }
        }
    }

    private static let stampFmt: DateFormatter = {
        let f = DateFormatter(); f.dateFormat = "MMM d HH:mm"; return f
    }()
    static func stamp(_ unix: Double) -> String {
        stampFmt.string(from: Date(timeIntervalSince1970: unix))
    }
    static func dayLabel(_ c: HourlyHR.Candle) -> String { String(c.ymd.suffix(5)) }
}

/// Candles on a real-time axis: every hour of the window gets a slot, so a gap in the
/// data is a gap in the chart. Wick = low…high, body = open…close (filled when the
/// hour ended higher than it started, hollow when it ended lower).
private struct CandleChart: View {
    let candles: [HourlyHR.Candle]
    let window: HourlyWindow
    @Binding var selected: HourlyHR.Candle?

    /// Slot start times: the newest hour anchors the right edge and the window runs
    /// backwards from it, one slot per hour whether or not it has beats.
    private var slots: [Double] {
        guard let newest = candles.last?.unix else { return [] }
        return (0..<window.hours).map { newest - Double((window.hours - 1 - $0) * 3600) }.sorted()
    }

    var body: some View {
        let byHour = Dictionary(uniqueKeysWithValues: candles.map { ($0.unix, $0) })
        let lo = candles.map(\.low).min() ?? 40
        let hi = candles.map(\.high).max() ?? 120
        let pad = max(hi - lo, 1) * 0.1
        let domainLo = lo - pad, domainHi = hi + pad
        let span = max(domainHi - domainLo, 1e-6)
        let slots = slots

        HStack(alignment: .top, spacing: 8) {
            VStack(alignment: .trailing, spacing: 0) {
                Text("\(Int(domainHi.rounded()))").font(Obs.mono(9)).foregroundStyle(Obs.muted)
                Spacer()
                Text("\(Int(((domainLo + domainHi) / 2).rounded()))").font(Obs.mono(9)).foregroundStyle(Obs.muted)
                Spacer()
                Text("\(Int(domainLo.rounded()))").font(Obs.mono(9)).foregroundStyle(Obs.muted)
            }
            .frame(width: 30)

            GeometryReader { geo in
                let width = geo.size.width
                let slotWidth = width / CGFloat(max(slots.count, 1))
                Canvas { ctx, size in
                    func y(_ v: Double) -> CGFloat {
                        size.height * (1 - CGFloat((v - domainLo) / span))
                    }
                    for fraction in [0.0, 0.5, 1.0] {
                        var grid = Path()
                        let gy = size.height * CGFloat(fraction)
                        grid.move(to: CGPoint(x: 0, y: gy))
                        grid.addLine(to: CGPoint(x: size.width, y: gy))
                        ctx.stroke(grid, with: .color(Obs.trace.opacity(0.4)), lineWidth: 0.5)
                    }
                    // midnight rules, so a multi-day window reads as days
                    for (i, start) in slots.enumerated() where Int(start.rounded()) % 86_400 == midnightOffset {
                        var rule = Path()
                        let x = slotWidth * CGFloat(i)
                        rule.move(to: CGPoint(x: x, y: 0))
                        rule.addLine(to: CGPoint(x: x, y: size.height))
                        ctx.stroke(rule, with: .color(Obs.trace), style: StrokeStyle(lineWidth: 0.6, dash: [3, 3]))
                    }

                    let bodyWidth = max(1.5, slotWidth * 0.62)
                    for (i, start) in slots.enumerated() {
                        guard let c = byHour[start] else { continue }
                        let cx = slotWidth * (CGFloat(i) + 0.5)
                        let isSelected = selected?.unix == start
                        let tint = isSelected ? Obs.ink : Obs.chart

                        var wick = Path()
                        wick.move(to: CGPoint(x: cx, y: y(c.high)))
                        wick.addLine(to: CGPoint(x: cx, y: y(c.low)))
                        ctx.stroke(wick, with: .color(tint.opacity(isSelected ? 1 : 0.75)), lineWidth: 1)

                        let top = min(y(c.open), y(c.close))
                        let bottom = max(y(c.open), y(c.close))
                        let body = CGRect(x: cx - bodyWidth / 2, y: top,
                                          width: bodyWidth, height: max(1.4, bottom - top))
                        let shape = Path(roundedRect: body, cornerRadius: min(2, bodyWidth / 3))
                        if c.rising {
                            ctx.fill(shape, with: .color(tint.opacity(isSelected ? 1 : 0.85)))
                        } else {
                            ctx.fill(shape, with: .color(Obs.paper))
                            ctx.stroke(shape, with: .color(tint), lineWidth: 1)
                        }
                    }
                }
                .contentShape(Rectangle())
                .gesture(
                    DragGesture(minimumDistance: 0)
                        .onChanged { g in
                            let i = Int(g.location.x / max(slotWidth, 0.001))
                            guard slots.indices.contains(i) else { return }
                            selected = byHour[slots[i]]
                        }
                        .onEnded { _ in }
                )
                .accessibilityLabel("Hourly heart rate candles, \(candles.count) hours with data")
            }
        }
    }

    /// A local-clock midnight is an exact multiple of a day once the tz offset is
    /// removed; the FFI keys buckets in UTC, so shift before testing.
    private var midnightOffset: Int {
        let tz = Int(HourlyHR.tzOffset) * 3600
        return ((-tz) % 86_400 + 86_400) % 86_400
    }
}
