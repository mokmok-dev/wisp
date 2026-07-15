import Foundation
@testable import WispAudioKit
import XCTest

final class WispSessionTests: XCTestCase {
    func testInitRejectsExistingMicOutput() throws {
        try assertInitRejectsExistingOutput("mic.wav")
    }

    func testInitRejectsExistingSystemOutput() throws {
        try assertInitRejectsExistingOutput("system.wav")
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

private func makeTemporaryDirectory() -> URL {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("WispAudioKitTests-\(UUID().uuidString)", isDirectory: true)
    try! FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}
