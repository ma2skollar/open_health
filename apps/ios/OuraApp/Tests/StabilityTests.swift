import XCTest
import SQLite3
@testable import OuraApp

final class StabilityTests: XCTestCase {
    func testOperationFailureSummaryKeepsCauseBeforeStackTrace() {
        let cause = "2026-09-09 error [models] activity day=2026-07-06 failed: select index out of range"
        let trace = (0..<24).map { "frame #\($0): libtorch_cpu" }.joined(separator: "\n")
        let preview = DiagStore.incidentPreview("[diag] omitted 2 queued records\n" + cause + "\n" + trace,
                                                kind: "operation-failure")
        XCTAssertTrue(preview.hasPrefix(cause))
        XCTAssertFalse(preview.contains("frame #23"))
        XCTAssertEqual(DiagStore.incidentPreview("", kind: "operation-failure"), "")
    }

    func testLeftoverSessionIsNotACrash() {
        XCTAssertEqual(DiagStore.classify("[models] complete"), "interrupted-session-cause-unknown")
        XCTAssertEqual(DiagStore.classify("[lifecycle] state=background"), "backgrounded-session")
        XCTAssertEqual(DiagStore.classify("[lifecycle] state=background\n[lifecycle] state=active"), "interrupted-session-cause-unknown")
        XCTAssertEqual(DiagStore.classify("[lifecycle] state=background\n*** CRASH ***\nsignal=11"), "confirmed-crash")
    }

    func testRotationIsBoundedAndDetailedExportIncludesNewestRecord() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = DiagStore(directory: directory, segmentLimit: 2048, totalLimit: 8192)
        for i in 0..<100 {
            store.append("record=\(i) " + String(repeating: "x", count: 512))
            store.flush()
        }
        let text = store.exportAll()
        XCTAssertTrue(text.contains("record=99"))
        XCTAssertFalse(text.contains("record=0 "))
        let enumerator = FileManager.default.enumerator(at: directory, includingPropertiesForKeys: [.fileSizeKey])!
        var bytes = 0
        for case let url as URL in enumerator { bytes += (try url.resourceValues(forKeys: [.fileSizeKey])).fileSize ?? 0 }
        XCTAssertLessThanOrEqual(bytes, 10240) // total limit plus the currently filling segment
    }

    func testCancelledGenerationStaysCancelledAcrossWorkerHop() {
        let run = AnalysisRun()
        run.cancel()
        run.perform { XCTAssertTrue(AnalysisRun.cancelled); do { try AnalysisRun.check(); XCTFail("cancelled run accepted") } catch {} }
        XCTAssertNil(AnalysisRun.current)
    }

    func testWorkGateSerializesOwners() async {
        let gate = WorkGate()
        actor Counter {
            var active = 0
            var peak = 0
            func enter() { active += 1; peak = max(peak, active) }
            func leave() { active -= 1 }
        }
        let counter = Counter()
        await withTaskGroup(of: Void.self) { group in
            for _ in 0..<20 {
                group.addTask {
                    await gate.acquire()
                    await counter.enter()
                    await Task.yield()
                    await counter.leave()
                    await gate.release()
                }
            }
        }
        let peak = await counter.peak
        XCTAssertEqual(peak, 1)
    }

    #if TORCH
    func testAutomaticSleepOnlyFillsLatestMissingNight() {
        let latest = NightRow(ymd: "2026-09-09", start_ds: 200, end_ds: 300, start: "23:00", end: "07:00")
        let older = NightRow(ymd: "2026-09-08", start_ds: 100, end_ds: 190, start: "23:00", end: "07:00")
        var savedLatest = latest
        savedLatest.stages = [1, 2, 3, 4]
        var savedOlder = older
        savedOlder.stages = [2, 3, 2, 1]
        let nights = [latest, older]

        let firstLaunch = Core.automaticSleepPlan(nights: nights, previous: nil)
        XCTAssertEqual(firstLaunch.pending.map(\.start_ds), [200])
        let missingLatest = Core.automaticSleepPlan(nights: nights, previous: Summary(nights: [savedOlder]))
        XCTAssertEqual(missingLatest.pending.map(\.start_ds), [200])
        XCTAssertEqual(missingLatest.saved["100"], savedOlder.stages)
        let missingOlder = Core.automaticSleepPlan(nights: nights, previous: Summary(nights: [savedLatest]))
        XCTAssertTrue(missingOlder.pending.isEmpty)
        let reopen = Core.automaticSleepPlan(nights: nights, previous: Summary(nights: [savedLatest, savedOlder]))
        XCTAssertTrue(reopen.pending.isEmpty)
        XCTAssertEqual(reopen.saved.count, 2)

        var changed = latest
        changed.end_ds = 310
        XCTAssertEqual(Core.automaticSleepPlan(nights: [changed, older], previous: Summary(nights: [savedLatest])).pending.count, 1)
        XCTAssertTrue(Core.automaticSleepPlan(nights: [], previous: nil).pending.isEmpty)
    }

    func testAutomaticSleepPicksTheNightWokenFromMostRecently() {
        // List order is not trusted: a misdated older night could sort first.
        var real = NightRow(ymd: "2026-09-10", start_ds: 900, end_ds: 990, start: "23:00", end: "08:00")
        real.wake_ymd = "2026-09-11"; real.end_unix = 1_789_020_000
        var older = NightRow(ymd: "2026-09-09", start_ds: 500, end_ds: 590, start: "23:30", end: "07:30")
        older.wake_ymd = "2026-09-10"; older.end_unix = 1_788_933_600
        XCTAssertEqual(Core.automaticSleepPlan(nights: [older, real], previous: nil).pending.map(\.start_ds), [900])
        // Without absolute bounds (older cached summary) the wake date decides.
        real.end_unix = nil; older.end_unix = nil
        XCTAssertEqual(Core.automaticSleepPlan(nights: [older, real], previous: nil).pending.map(\.start_ds), [900])
    }

    func testWakeDateComesFromTheSharedBrainWhenPresent() {
        var night = NightRow(ymd: "2026-09-10", start_ds: 1, end_ds: 2, start: "07:11", end: "15:33")
        XCTAssertEqual(Summary().wakeYmd(night), "2026-09-10")   // legacy heuristic: no midnight crossing
        night.wake_ymd = "2026-09-11"
        XCTAssertEqual(Summary().wakeYmd(night), "2026-09-11")
    }

    func testHistoricalActivityRefreshBypassesCacheAndPreservesOtherDays() throws {
        let url = try fixture()
        defer { try? FileManager.default.removeItem(at: url) }
        let file = "test-activity-\(UUID().uuidString).json"
        let cacheURL = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(file)
        defer { try? FileManager.default.removeItem(at: cacheURL) }
        var db: OpaquePointer?
        XCTAssertEqual(sqlite3_open(url.path, &db), SQLITE_OK)
        let calendar = Calendar.current
        let midnight = calendar.date(from: DateComponents(year: 2026, month: 7, day: 6))!
        let next = calendar.date(byAdding: .day, value: 1, to: midnight)!
        let unix = Int64(midnight.timeIntervalSince1970)
        let nextUnix = Int64(next.timeIntervalSince1970)
        let met = String(data: try JSONSerialization.data(withJSONObject: ["met": Array(repeating: 1.2, count: 720)]), encoding: .utf8)!
        let sql = """
        DELETE FROM events;
        INSERT INTO events VALUES (1,5000000,66,'{"unix_time":\(unix)}',\(unix),NULL);
        INSERT INTO events VALUES (2,5000000,80,'\(met)',\(unix),NULL);
        INSERT INTO events VALUES (3,\(5000000 + (nextUnix - unix) * 10),80,'\(met)',\(nextUnix),NULL);
        """
        XCTAssertEqual(sqlite3_exec(db, sql, nil, nil, nil), SQLITE_OK)
        sqlite3_close(db)
        let events = try EventStore.decodedEvents(dbPath: url.path)
        let clock = EventStore.RingClock(events: events)
        let key = ModelCacheStore.globalKey(profile: nil)
        let unrelated = ActivityDayEntry(fp: "preserve-this-day", sessions: [])
        ModelCacheStore.save(file, globalKey: key, entries: ["2026-07-07": unrelated])

        let first = ActivityModel.run(profile: nil, events: events, clock: clock,
                                      onlyDay: "2026-07-06", force: true, cacheFile: file)
        XCTAssertNil(first.error)
        var cache: [String: ActivityDayEntry] = ModelCacheStore.load(file, globalKey: key)
        XCTAssertEqual(cache["2026-07-07"]?.fp, unrelated.fp)
        var old = try XCTUnwrap(cache["2026-07-06"])
        old.sessions = [WorkoutSession(start: "2026-07-06 01:00", end: "01:10", durationMin: 10,
                                       label: "stale cached result", isWorkout: 1)]
        cache["2026-07-06"] = old
        ModelCacheStore.save(file, globalKey: key, entries: cache)
        let cached = ActivityModel.run(profile: nil, events: events, clock: clock,
                                       onlyDay: "2026-07-06", cacheFile: file)
        XCTAssertEqual(cached.sessions.first?.label, "stale cached result")
        let refreshed = ActivityModel.run(profile: nil, events: events, clock: clock,
                                          onlyDay: "2026-07-06", force: true, cacheFile: file)
        XCTAssertNil(refreshed.error)
        XCTAssertTrue(refreshed.sessions.isEmpty)
        cache = ModelCacheStore.load(file, globalKey: key)
        XCTAssertTrue(try XCTUnwrap(cache["2026-07-06"]).sessions.isEmpty)
        XCTAssertEqual(cache["2026-07-07"]?.fp, unrelated.fp)
        let missing = ActivityModel.run(profile: nil, events: events, clock: clock,
                                        onlyDay: "2026-07-05", force: true, cacheFile: file)
        XCTAssertNotNil(missing.error)
    }

    func testActivitySparseDayReturnsNoWorkouts() throws {
        let path = try XCTUnwrap(Bundle.main.path(forResource: "automatic_activity_detection_3_1_11", ofType: "ptl"))
        let nan = Float.nan
        var context: [Float] = [2026, 9, 8, 1]
        var user: [Float] = [30, 1, 1.78, 75] + Array(repeating: nan, count: 10)
        var step: [Float] = [0] + Array(repeating: nan, count: 11) + [719] + Array(repeating: nan, count: 11)
        var motion: [Float] = [0] + Array(repeating: nan, count: 8)
        var temp: [Float] = [0, nan]
        var hr: [Float] = [0, nan]
        for duplicate in [false, true] {
            var met: [Float] = []
            for minute in 0..<720 {
                met += [Float(minute), 1.2]
                if duplicate && minute < 60 { met += [Float(minute), 1.2] }
            }
            let rows = Int32(met.count / 2)
            var output = [Float](repeating: 0, count: 512 * 9)
            let result = oura_activity(path, &context, &user, &met, rows, &step, 2,
                                       &motion, 1, &temp, 1, &hr, 1, 0.5, 10, &output, 512)
            XCTAssertEqual(result, 0, "Sparse day should produce no workouts, not fail: \(String(cString: oura_activity_last_error()))")
        }
    }

    func testActivityCompleteDayRunsInMobileRuntime() throws {
        let path = try XCTUnwrap(Bundle.main.path(forResource: "automatic_activity_detection_3_1_11", ofType: "ptl"))
        let nan = Float.nan
        var context: [Float] = [2026, 9, 8, 1]
        var user: [Float] = [30, 1, 1.78, 75] + Array(repeating: nan, count: 10)
        var step: [Float] = [0] + Array(repeating: nan, count: 11) + [719] + Array(repeating: nan, count: 11)
        var met: [Float] = [], motion: [Float] = [], temp: [Float] = [], hr: [Float] = []
        for minute in 0..<720 {
            let t = Float(minute)
            met += [t, (300..<360).contains(minute) ? 5 : 1.2]
            motion += [t, 0, 30, 0, 0, 0, nan, 10, 1]
            temp += [t, 33]
            hr += [t, 70]
        }
        var output = [Float](repeating: 0, count: 512 * 9)
        let result = oura_activity(path, &context, &user, &met, 720, &step, 2,
                                   &motion, 720, &temp, 720, &hr, 720, 0.5, 10, &output, 512)
        // Golden result from the unmodified full TorchScript model on these inputs.
        let expected: [Float] = [300, 360, 0.8837792, 14, 0.5082318, 6, 0.4652145, 21, 0.4478024]
        XCTAssertEqual(result, 1, String(cString: oura_activity_last_error()))
        for (actual, reference) in zip(output.prefix(9), expected) {
            XCTAssertEqual(actual, reference, accuracy: 0.0001)
        }
    }

    private func fixture() throws -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".db")
        var db: OpaquePointer?
        XCTAssertEqual(sqlite3_open(url.path, &db), SQLITE_OK)
        defer { sqlite3_close(db) }
        let sql = """
        CREATE TABLE events(id INTEGER PRIMARY KEY, ring_timestamp INTEGER, tag INTEGER, decoded_json TEXT, captured_unix INTEGER, body BLOB);
        INSERT INTO events VALUES (1,5000000,66,'{"unix_time":1700000000}',1700000000,NULL);
        INSERT INTO events VALUES (2,5000010,96,'{"ibi_ms":[800,900]}',1700000001,NULL);
        INSERT INTO events VALUES (3,10,66,'{"unix_time":1700100000}',1700100000,NULL);
        INSERT INTO events VALUES (4,20,96,'{"ibi_ms":[700,800]}',1700100001,NULL);
        """
        XCTAssertEqual(sqlite3_exec(db, sql, nil, nil, nil), SQLITE_OK)
        return url
    }
    func testIllnessWorkspacePreservesCaptureOrderAndInclusiveWindows() throws {
        let workspace = try IllnessModel.IBIWorkspace()
        defer { workspace.close() }
        try workspace.append(time: 11, value: 800)
        try workspace.append(time: 5, value: 900)
        try workspace.append(time: 11, value: 700)
        try workspace.append(time: 30, value: 600)
        try workspace.finish()
        let values = try workspace.values(start: 5, end: 11)
        XCTAssertEqual(values.times, [11,5,11])
        XCTAssertEqual(values.values, [800,900,700])
        XCTAssertTrue(try workspace.values(start: 12, end: 20).times.isEmpty)
    }

    func testCvaRetainsExactlyLast4000Segments() throws {
        let url = try fixture(); defer { try? FileManager.default.removeItem(at: url) }
        var db: OpaquePointer?
        XCTAssertEqual(sqlite3_open(url.path, &db), SQLITE_OK)
        XCTAssertEqual(sqlite3_exec(db, "BEGIN;", nil, nil, nil), SQLITE_OK)
        var statement: OpaquePointer?
        XCTAssertEqual(sqlite3_prepare_v2(db, "INSERT INTO events(ring_timestamp,tag,body) VALUES (?,129,?)", -1, &statement, nil), SQLITE_OK)
        let bytes = [UInt8](repeating: 1, count: 1500)
        for i in 0..<4002 {
            sqlite3_bind_int64(statement, 1, Int64(i * 10))
            _ = bytes.withUnsafeBytes { sqlite3_bind_blob(statement, 2, $0.baseAddress, 1500, unsafeBitCast(-1, to: sqlite3_destructor_type.self)) }
            XCTAssertEqual(sqlite3_step(statement), SQLITE_DONE)
            sqlite3_reset(statement)
        }
        sqlite3_finalize(statement)
        XCTAssertEqual(sqlite3_exec(db, "COMMIT;", nil, nil, nil), SQLITE_OK)
        sqlite3_close(db)
        let result = CvaModel.selectedSegments(dbPath: url.path)
        XCTAssertNil(result.error)
        XCTAssertEqual(result.segments?.count, 4000 * 1500)
        XCTAssertEqual(result.segments?.first, 3001)
        XCTAssertEqual(result.segments?.last, Float(4002 * 1500))
    }

    func testRtcBeaconDatesSleepIndependentlyOfDownloadTime() throws {
        for previousBoot in [false, true] {
            let url = try fixture()
            defer { try? FileManager.default.removeItem(at: url) }
            var db: OpaquePointer?
            XCTAssertEqual(sqlite3_open(url.path, &db), SQLITE_OK)
            let sql = """
            DELETE FROM events;
            \(previousBoot ? "INSERT INTO events VALUES (1,5000000,66,'{\"unix_time\":1788800000}',1788800000,NULL);" : "")
            INSERT INTO events VALUES (2,672400,1,'{}',1789056420,NULL);
            INSERT INTO events VALUES (3,1000000,133,'{"unix_time":1789020420}',1789056420,NULL);
            """
            XCTAssertEqual(sqlite3_exec(db, sql, nil, nil, nil), SQLITE_OK)
            sqlite3_close(db)
            let events = try EventStore.decodedEvents(dbPath: url.path)
            let clock = EventStore.RingClock(events: events)
            // UTC+1: Sep 9 22:01 -> Sep 10 07:07, downloaded at 17:07.
            XCTAssertEqual(clock.unixSeconds(672400, capturedUnix: 1789056420), 1788987660)
            XCTAssertEqual(clock.unixSeconds(1000000, capturedUnix: 1789056420), 1789020420)
            XCTAssertEqual(clock.latestUnix, 1789020420)
            if previousBoot {
                XCTAssertEqual(clock.unixSeconds(5000000, capturedUnix: 1788800000), 1788800000)
            }
            try events.validate()
        }
    }

    func testUnanchoredBootIsUndatedInsteadOfDatedToDownloadTime() throws {
        // A rebooted ring drained in one go with no time_sync/rtc_beacon: dating the
        // night to the download would show 07:11→15:33 instead of 23:00→08:00, and
        // projecting it through the previous boot's clock would land it days earlier.
        let url = try fixture()
        defer { try? FileManager.default.removeItem(at: url) }
        var db: OpaquePointer?
        XCTAssertEqual(sqlite3_open(url.path, &db), SQLITE_OK)
        let sql = """
        DELETE FROM events;
        INSERT INTO events VALUES (1,5000000,66,'{"unix_time":1788800000}',1788800000,NULL);
        INSERT INTO events VALUES (2,5100000,1,'{}',1788800000,NULL);
        INSERT INTO events VALUES (3,10,1,'{}',1789056420,NULL);
        INSERT INTO events VALUES (4,300000,118,'{"bedtime_start_ds":300000}',1789056420,NULL);
        INSERT INTO events VALUES (5,700000,1,'{}',1789056420,NULL);
        """
        XCTAssertEqual(sqlite3_exec(db, sql, nil, nil, nil), SQLITE_OK)
        sqlite3_close(db)
        let events = try EventStore.decodedEvents(dbPath: url.path)
        let clock = EventStore.RingClock(events: events)
        let resolved = clock.resolve(300000, capturedUnix: 1789056420)
        XCTAssertEqual(resolved.source, .undated)
        XCTAssertFalse(resolved.source.isDated)
        XCTAssertNotEqual(resolved.unix, 1788800000 + Double(300000 - 5000000) / 10)
        XCTAssertEqual(clock.resolve(5100000, capturedUnix: 1788800000).source, .anchor)
        try events.validate()
    }

    func testPhoneAnchorDatesANewBoot() throws {
        // The sync wrote a phone-time anchor at the newest drained ds: 23:00→08:00 UTC+2.
        let url = try fixture()
        defer { try? FileManager.default.removeItem(at: url) }
        var db: OpaquePointer?
        XCTAssertEqual(sqlite3_open(url.path, &db), SQLITE_OK)
        let sql = """
        DELETE FROM events;
        INSERT INTO events VALUES (1,5000000,66,'{"unix_time":1788800000}',1788800000,NULL);
        INSERT INTO events VALUES (2,10,1,'{}',1789056420,NULL);
        INSERT INTO events VALUES (3,705000,66,'{"unix_time":1789056420,"source":"phone"}',1789056420,NULL);
        """
        XCTAssertEqual(sqlite3_exec(db, sql, nil, nil, nil), SQLITE_OK)
        sqlite3_close(db)
        let events = try EventStore.decodedEvents(dbPath: url.path)
        let clock = EventStore.RingClock(events: events)
        let startDs: Int64 = 705000 - (1789056420 - 1789002000) * 10
        let endDs: Int64 = 705000 - (1789056420 - 1789020000) * 10
        XCTAssertEqual(clock.resolve(startDs, capturedUnix: 1789056420).source, .anchor)
        XCTAssertEqual(clock.unixSeconds(startDs, capturedUnix: 1789056420), 1789002000)
        XCTAssertEqual(clock.unixSeconds(endDs, capturedUnix: 1789056420), 1789020000)
        try events.validate()
    }

    func testStreamIsRepeatableAndKeepsRebootOrder() throws {
        let url = try fixture(); defer { try? FileManager.default.removeItem(at: url) }
        let events = try EventStore.decodedEvents(dbPath: url.path)
        XCTAssertEqual(events.map(\.ds), [5000000,5000010,10,20])
        XCTAssertEqual(events.restricted("tag=96").map(\.ds), [5000010,20])
        let clock = EventStore.RingClock(events: events)
        XCTAssertEqual(clock.unixSeconds(20, capturedUnix: 1700100001), 1700100001)
        XCTAssertEqual(events.map(\.ds).count, 4)
        try events.validate()
    }
    func testStreamCancellationAndReadFailureAreNotEmptySuccess() throws {
        let url = try fixture(); defer { try? FileManager.default.removeItem(at: url) }
        let events = try EventStore.decodedEvents(dbPath: url.path)
        let run = AnalysisRun()
        run.perform {
            let iterator = events.makeIterator()
            XCTAssertNotNil(iterator.next())
            run.cancel()
            XCTAssertNil(iterator.next())
            do { try events.validate(); XCTFail("partial stream accepted") } catch {}
        }
        XCTAssertThrowsError(try EventStore.decodedEvents(dbPath: url.path + ".missing"))
    }
    #endif
}
