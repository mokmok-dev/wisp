import AVFoundation
import Foundation
@testable import WispAudioKit
import XCTest

final class WispSessionTests: XCTestCase {
    func testInitRejectsExistingMicOutput() throws {
        try assertInitRejectsExistingOutput("mic.ogg")
    }

    func testInitRejectsExistingSystemOutput() throws {
        try assertInitRejectsExistingOutput("system.ogg")
    }

    func testConcurrentStopIsIdempotentBeforeStart() async throws {
        let outputDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: outputDir) }
        let session = try WispSession(outputDir: outputDir, onResult: { _ in })

        await withTaskGroup(of: Void.self) { group in
            group.addTask { await session.stop() }
            group.addTask { await session.stop() }
        }
    }

    func testMicrophoneMuteCanBeToggledBeforeStart() throws {
        let outputDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: outputDir) }
        let session = try WispSession(outputDir: outputDir, onResult: { _ in })

        XCTAssertFalse(session.isMicrophoneMuted)
        session.setMicrophoneMuted(true)
        XCTAssertTrue(session.isMicrophoneMuted)
        session.setMicrophoneMuted(false)
        XCTAssertFalse(session.isMicrophoneMuted)
    }

    func testStartAfterPreStartStopIsRejectedWithoutRequestingPermissions() async throws {
        let outputDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: outputDir) }
        let session = try WispSession(outputDir: outputDir, onResult: { _ in })

        await session.stop()

        do {
            try await session.start()
            XCTFail("start after stop unexpectedly succeeded")
        } catch let error as PoCError {
            guard case .invalidLifecycle(let message) = error else {
                XCTFail("Unexpected PoCError: \(error)")
                return
            }
            XCTAssertTrue(message.contains("already stopped"))
        } catch {
            XCTFail("Unexpected error type: \(error)")
        }
    }

    func testProcessTapStopIsIdempotentBeforeStart() {
        let capture = ProcessTapCapture { _ in }
        capture.stop()
        capture.stop()
    }

    func testResultSegmentIDsKeepVolatileAndFinalTogether() {
        var ids = ResultSegmentIDs()

        XCTAssertEqual(ids.id(isFinal: false), 1)
        XCTAssertEqual(ids.id(isFinal: false), 1)
        XCTAssertEqual(ids.id(isFinal: true), 1)
        XCTAssertEqual(ids.id(isFinal: false), 2)
        XCTAssertEqual(ids.id(isFinal: true), 2)
    }

    func testResultSegmentIDsHandleFinalOnlyResults() {
        var ids = ResultSegmentIDs()

        XCTAssertEqual(ids.id(isFinal: true), 1)
        XCTAssertEqual(ids.id(isFinal: true), 2)
    }

    func testRecorderWritesContinuousOggOpusPages() async throws {
        let outputDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: outputDir) }
        let output = outputDir.appendingPathComponent("test.ogg")
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 1,
            interleaved: false
        ))
        let recorder = try OpusOggRecorder(url: output, sourceFormat: format)
        for _ in 0 ..< 4 {
            let buffer = try XCTUnwrap(AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 4800))
            buffer.frameLength = 4800
            recorder.push(buffer)
        }
        await recorder.finish()

        let pages = try parseOggPages(Data(contentsOf: output))
        XCTAssertGreaterThan(pages.count, 3)
        XCTAssertEqual(pages[0].flags, 0x02)
        XCTAssertEqual(pages[0].packet.prefix(8), Data("OpusHead".utf8))
        XCTAssertEqual(pages[1].packet.prefix(8), Data("OpusTags".utf8))
        XCTAssertEqual(pages.last?.flags, 0x04)
        XCTAssertEqual(
            pages.map(\.sequence),
            Array(0 ..< UInt32(pages.count))
        )
        XCTAssertTrue(pages.allSatisfy(\.hasValidCRC))
    }

    func testReentrantBridgeStopReturnsInsteadOfDeadlocking() throws {
        let outputDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: outputDir) }
        let callbacks = CallbackCoordinator()
        let session = try WispSession(outputDir: outputDir, onResult: { _ in })
        let handle = SessionHandle(session: session, callbacks: callbacks)
        let rawPointer = Unmanaged.passRetained(handle).toOpaque()
        let pointerBits = UInt(bitPattern: rawPointer)
        let returned = expectation(description: "reentrant stop returned")

        DispatchQueue.global(qos: .userInitiated).async {
            callbacks.invoke {
                wisp_session_stop(session: OpaquePointer(bitPattern: pointerBits))
                returned.fulfill()
            }
        }

        let result = XCTWaiter.wait(for: [returned], timeout: 2)
        guard result == .completed else {
            // Do not enter another lifecycle call on the failure path: the
            // behavior under test may still be blocked in the callback.
            XCTFail("wisp_session_stop deadlocked in its own callback")
            return
        }
        wisp_session_free(session: OpaquePointer(bitPattern: pointerBits))
    }

    private func assertInitRejectsExistingOutput(_ fileName: String) throws {
        let outputDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: outputDir) }
        try Data().write(to: outputDir.appendingPathComponent(fileName))

        XCTAssertThrowsError(
            try WispSession(outputDir: outputDir, onResult: { _ in })
        ) { error in
            guard let error = error as? PoCError else {
                XCTFail("Unexpected error type: \(error)")
                return
            }
            guard case .outputFilesAlreadyExist(let path) = error else {
                XCTFail("Unexpected PoCError: \(error)")
                return
            }
            XCTAssertEqual(path, outputDir.path)
        }
    }
}

private struct ParsedOggPage {
    let flags: UInt8
    let sequence: UInt32
    let packet: Data
    let hasValidCRC: Bool
}

private func parseOggPages(_ data: Data) throws -> [ParsedOggPage] {
    var pages: [ParsedOggPage] = []
    var offset = 0
    while offset < data.count {
        guard offset + 27 <= data.count,
              data[offset ..< offset + 4] == Data("OggS".utf8)
        else {
            throw CocoaError(.fileReadCorruptFile)
        }
        let segmentCount = Int(data[offset + 26])
        guard offset + 27 + segmentCount <= data.count else {
            throw CocoaError(.fileReadCorruptFile)
        }
        let bodyLength = data[
            offset + 27 ..< offset + 27 + segmentCount
        ].reduce(0) { $0 + Int($1) }
        let end = offset + 27 + segmentCount + bodyLength
        guard end <= data.count else {
            throw CocoaError(.fileReadCorruptFile)
        }

        var page = Data(data[offset ..< end])
        let expectedCRC = page.readLittleEndianUInt32(at: 22)
        page.replaceSubrange(22 ..< 26, with: repeatElement(UInt8(0), count: 4))
        pages.append(ParsedOggPage(
            flags: data[offset + 5],
            sequence: data.readLittleEndianUInt32(at: offset + 18),
            packet: Data(data[offset + 27 + segmentCount ..< end]),
            hasValidCRC: page.oggTestCRC == expectedCRC
        ))
        offset = end
    }
    return pages
}

private extension Data {
    func readLittleEndianUInt32(at offset: Int) -> UInt32 {
        UInt32(self[offset])
            | UInt32(self[offset + 1]) << 8
            | UInt32(self[offset + 2]) << 16
            | UInt32(self[offset + 3]) << 24
    }

    var oggTestCRC: UInt32 {
        var crc: UInt32 = 0
        for byte in self {
            crc ^= UInt32(byte) << 24
            for _ in 0 ..< 8 {
                crc = (crc & 0x8000_0000) != 0
                    ? (crc << 1) ^ 0x04C1_1DB7
                    : crc << 1
            }
        }
        return crc
    }
}

private func makeTemporaryDirectory() -> URL {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("WispAudioKitTests-\(UUID().uuidString)", isDirectory: true)
    try! FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}
