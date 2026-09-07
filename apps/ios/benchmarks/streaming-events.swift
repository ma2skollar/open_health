import Foundation
// The production reader checks this token; benchmark runs are never cancelled.
enum AnalysisRun {
    static let cancelled = false
    static func check() throws {}
}
@main struct Benchmark {
    static func main() throws {
        let started = ProcessInfo.processInfo.systemUptime
        let events = try EventStore.decodedEvents(dbPath: CommandLine.arguments[1])
        var count = 0
        var checksum: Int64 = 0
        for event in events { count += 1; checksum &+= event.ds }
        let clock = EventStore.RingClock(events: events)
        print("rows=\(count) checksum=\(checksum) anchor=\(clock.latestUnix) seconds=\(ProcessInfo.processInfo.systemUptime - started)")
    }
}
