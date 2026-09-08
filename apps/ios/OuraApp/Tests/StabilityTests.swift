import XCTest
import SQLite3
@testable import OuraApp

final class StabilityTests: XCTestCase {
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
