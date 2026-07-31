import AVFoundation
import Foundation
import os.lock
@testable import WispAudioKit
import XCTest

final class WispSessionTests: XCTestCase {
    func testInitRejectsExistingMicOutput() throws {
        try assertInitRejectsExistingOutput("mic.ogg")
    }

    func testInitRejectsExistingSystemOutput() throws {
        try assertInitRejectsExistingOutput("system.ogg")
    }

    func testLegacyAndV2ConstructorsSelectExclusiveAnalyzerInputOwners() throws {
        let resultCallback: WispResultCallback = {
            _, _, _, _, _, _, _, _, _, _ in
        }
        let legacyOutput = makeTemporaryDirectory()
        let v2Output = makeTemporaryDirectory()
        defer {
            try? FileManager.default.removeItem(at: legacyOutput)
            try? FileManager.default.removeItem(at: v2Output)
        }

        let legacyPointer = legacyOutput.path.withCString { output in
            "ja-JP".withCString { locale in
                wisp_session_new(
                    output_dir: output,
                    locale: locale,
                    on_result: resultCallback,
                    on_log: nil,
                    user_data: nil
                )
            }
        }
        let legacy = try XCTUnwrap(legacyPointer)
        let legacyHandle = Unmanaged<SessionHandle>
            .fromOpaque(UnsafeRawPointer(legacy))
            .takeUnretainedValue()
        XCTAssertTrue(legacyHandle.session.feedsCapturedAudioDirectlyToAnalyzer)
        wisp_session_free(session: legacy)

        let v2Pointer = v2Output.path.withCString { output in
            "ja-JP".withCString { locale in
                wisp_session_new_v2(
                    output_dir: output,
                    locale: locale,
                    transcription_enabled: 1,
                    allow_record_only: 0,
                    on_result: resultCallback,
                    on_audio: nil,
                    on_audio_overflow: nil,
                    on_transcriber_error: nil,
                    on_terminal_error: nil,
                    on_log: nil,
                    user_data: nil
                )
            }
        }
        let v2 = try XCTUnwrap(v2Pointer)
        let v2Handle = Unmanaged<SessionHandle>
            .fromOpaque(UnsafeRawPointer(v2))
            .takeUnretainedValue()
        XCTAssertFalse(v2Handle.session.feedsCapturedAudioDirectlyToAnalyzer)
        wisp_session_free(session: v2)
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

    func testCaptureMilestoneSurvivesLaterFailureCleanup() {
        let milestone = CaptureMilestone()

        XCTAssertFalse(milestone.reached)
        milestone.markReached()
        XCTAssertTrue(milestone.reached)

        // Failed-start cleanup must not erase evidence that capture ran.
        XCTAssertTrue(milestone.reached)
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

    func testSessionAudioClockRemainsContinuousAcrossSampleRateChanges() {
        var clock = SessionAudioClock()
        let first = clock.next(
            source: .mic,
            frameCount: 441,
            sampleRate: 44100,
            hostNanoseconds: 1_000_000_000
        )
        let second = clock.next(
            source: .mic,
            frameCount: 480,
            sampleRate: 48000,
            hostNanoseconds: 1_010_000_000
        )
        let third = clock.next(
            source: .mic,
            frameCount: 960,
            sampleRate: 96000,
            hostNanoseconds: 1_020_000_000
        )

        XCTAssertEqual(first.timestamp, 0, accuracy: 0.000_001)
        XCTAssertEqual(second.timestamp, 0.01, accuracy: 0.000_001)
        XCTAssertEqual(third.timestamp, 0.02, accuracy: 0.000_001)
        XCTAssertEqual([first.sequence, second.sequence, third.sequence], [0, 1, 2])
    }

    func testSessionAudioClockKeepsDelayedSystemTrackOnSessionTimeline() {
        var clock = SessionAudioClock()
        _ = clock.next(
            source: .mic,
            frameCount: 480,
            sampleRate: 48000,
            hostNanoseconds: 2_000_000_000
        )
        let system = clock.next(
            source: .system,
            frameCount: 480,
            sampleRate: 48000,
            hostNanoseconds: 2_510_000_000
        )
        let nextMic = clock.next(
            source: .mic,
            frameCount: 480,
            sampleRate: 48000,
            hostNanoseconds: 2_520_000_000
        )

        XCTAssertEqual(system.timestamp, 0.5, accuracy: 0.000_001)
        XCTAssertEqual(nextMic.timestamp, 0.51, accuracy: 0.000_001)
        XCTAssertEqual(clock.trackStartSeconds(source: .mic), 0, accuracy: 0.000_001)
        XCTAssertEqual(clock.trackStartSeconds(source: .system), 0.5, accuracy: 0.000_001)
        XCTAssertEqual(system.sequence, 0)
        XCTAssertEqual(nextMic.sequence, 1)
    }

    func testSessionAudioClockPreservesTrackOffsetWhenFramesDrainInReverseOrder() {
        var clock = SessionAudioClock()
        clock.anchor(at: 2_000_000_000)

        // The system queue drains first even though its frame was captured
        // half a second after the microphone frame.
        let system = clock.next(
            source: .system,
            frameCount: 480,
            sampleRate: 48000,
            hostNanoseconds: 2_510_000_000
        )
        let microphone = clock.next(
            source: .mic,
            frameCount: 480,
            sampleRate: 48000,
            hostNanoseconds: 2_010_000_000
        )

        XCTAssertEqual(system.timestamp, 0.5, accuracy: 0.000_001)
        XCTAssertEqual(microphone.timestamp, 0, accuracy: 0.000_001)
        XCTAssertEqual(clock.trackStartSeconds(source: .system), 0.5, accuracy: 0.000_001)
        XCTAssertEqual(clock.trackStartSeconds(source: .mic), 0, accuracy: 0.000_001)
    }

    func testExternalPushAdmissionClosesBeforeWaitingForAdmittedPush() async {
        let barrier = ExternalPushAdmissionBarrier()
        barrier.open()
        XCTAssertTrue(barrier.admit())
        barrier.close()
        XCTAssertFalse(barrier.admit(), "finish must reject every later ABI push")

        let finished = OSAllocatedUnfairLock(initialState: false)
        let waiter = Task {
            await barrier.waitUntilDrained()
            finished.withLock { $0 = true }
        }
        await Task.yield()
        XCTAssertFalse(finished.withLock { $0 })

        barrier.release()
        await waiter.value
        XCTAssertTrue(finished.withLock { $0 })
    }

    func testAnalyzerCoordinatorFinalizeErrorIsRetryableWithoutDiscardingFinals() async throws {
        enum InjectedFailure: Error {
            case firstAttempt
        }
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 1,
            interleaved: false
        ))
        let attempts = OSAllocatedUnfairLock(initialState: 0)
        let finals = OSAllocatedUnfairLock(initialState: [String]())
        let coordinator = AnalyzerCoordinator(
            label: "TEST",
            sourceFormat: format,
            onResult: { _ in },
            onError: { _, _ in },
            finalizeOperation: { _ in
                let attempt = attempts.withLock {
                    $0 += 1
                    return $0
                }
                if attempt == 1 {
                    finals.withLock { $0.append("final-before-error") }
                    throw InjectedFailure.firstAttempt
                }
            }
        )

        do {
            try await coordinator.finish()
            XCTFail("injected finalize failure unexpectedly succeeded")
        } catch InjectedFailure.firstAttempt {
            // Expected: the coordinator retains ownership for retry.
        }
        XCTAssertEqual(finals.withLock { $0 }, ["final-before-error"])
        try await coordinator.finish()
        XCTAssertEqual(attempts.withLock { $0 }, 2)
        XCTAssertEqual(finals.withLock { $0 }, ["final-before-error"])
    }

    func testAnalyzerFinishRetryRunsOnlyTheUnfinishedTrack() async throws {
        enum InjectedFailure: Error {
            case microphone
        }
        let progress = OSAllocatedUnfairLock(initialState: AnalyzerFinishProgress())
        let microphoneAttempts = OSAllocatedUnfairLock(initialState: 0)
        let systemAttempts = OSAllocatedUnfairLock(initialState: 0)
        let microphone: @Sendable () async throws -> Void = {
            let attempt = microphoneAttempts.withLock {
                $0 += 1
                return $0
            }
            if attempt == 1 {
                throw InjectedFailure.microphone
            }
        }
        let system: @Sendable () async throws -> Void = {
            systemAttempts.withLock { $0 += 1 }
        }

        do {
            try await finishAnalyzerOperations(
                progress: progress,
                microphone: microphone,
                system: system
            )
            XCTFail("injected microphone failure unexpectedly succeeded")
        } catch {
            XCTAssertTrue("\(error)".contains("microphone"))
        }
        XCTAssertEqual(microphoneAttempts.withLock { $0 }, 1)
        XCTAssertEqual(systemAttempts.withLock { $0 }, 1)

        try await finishAnalyzerOperations(
            progress: progress,
            microphone: microphone,
            system: system
        )
        XCTAssertEqual(microphoneAttempts.withLock { $0 }, 2)
        XCTAssertEqual(systemAttempts.withLock { $0 }, 1)
    }

    func testRealtimeHandoffReportsPoolOverflowWithoutBlockingProducer() throws {
        let enteredConsumer = expectation(description: "consumer owns sole slot")
        let overflowReported = expectation(description: "pool overflow reported")
        let releaseConsumer = DispatchSemaphore(value: 0)
        let handoff = RealtimeAudioHandoff(
            source: .mic,
            slotCount: 1,
            onBuffer: { _ in
                enteredConsumer.fulfill()
                releaseConsumer.wait()
            },
            onOverflow: { source, frames in
                XCTAssertEqual(source, .mic)
                XCTAssertEqual(frames, 480)
                overflowReported.fulfill()
            }
        )
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 1,
            interleaved: false
        ))
        let buffer = try XCTUnwrap(AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: 480
        ))
        buffer.frameLength = 480

        handoff.enqueue(buffer, muted: false)
        wait(for: [enteredConsumer], timeout: 2)
        handoff.enqueue(buffer, muted: false)
        releaseConsumer.signal()
        wait(for: [overflowReported], timeout: 2)
        handoff.drainSynchronously()
    }

    func testRealtimeHandoffPublishesOverflowBeforePostGapPCM() throws {
        let enteredConsumer = expectation(description: "pre-gap PCM entered consumer")
        let overflowReported = expectation(description: "gap published")
        let releaseConsumer = DispatchSemaphore(value: 0)
        let events = OSAllocatedUnfairLock<[String]>(initialState: [])
        let handoff = RealtimeAudioHandoff(
            source: .mic,
            slotCount: 1,
            onBuffer: { buffer in
                let marker = Int(buffer.samples[0])
                events.withLock { $0.append("pcm:\(marker)") }
                if marker == 1 {
                    enteredConsumer.fulfill()
                    releaseConsumer.wait()
                }
            },
            onOverflow: { _, frames in
                events.withLock { $0.append("gap:\(frames)") }
                overflowReported.fulfill()
            }
        )
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 1,
            interleaved: false
        ))
        func buffer(_ marker: Float) throws -> AVAudioPCMBuffer {
            let value = try XCTUnwrap(AVAudioPCMBuffer(
                pcmFormat: format,
                frameCapacity: 2
            ))
            value.frameLength = 2
            value.floatChannelData?[0][0] = marker
            value.floatChannelData?[0][1] = marker
            return value
        }

        try handoff.enqueue(buffer(1), muted: false)
        wait(for: [enteredConsumer], timeout: 2)
        try handoff.enqueue(buffer(2), muted: false)
        try handoff.enqueue(buffer(3), muted: false)
        releaseConsumer.signal()
        wait(for: [overflowReported], timeout: 2)
        handoff.drainSynchronously()
        try handoff.enqueue(buffer(4), muted: false)
        handoff.drainSynchronously()

        XCTAssertEqual(events.withLock { $0 }, ["pcm:1", "gap:4", "pcm:4"])
    }

    func testRealtimeHandoffClosesAdmissionRaceAndAccountsEveryPendingDrop() throws {
        let enteredConsumer = expectation(description: "pre-gap PCM entered consumer")
        let enteredRacingCommit = expectation(description: "racing producer reached commit")
        let racingCommitReturned = expectation(description: "racing producer returned")
        let overflowReported = expectation(description: "complete gap published")
        let releaseConsumer = DispatchSemaphore(value: 0)
        let releaseRacingCommit = DispatchSemaphore(value: 0)
        let admissionCount = OSAllocatedUnfairLock<Int>(initialState: 0)
        let events = OSAllocatedUnfairLock<[String]>(initialState: [])
        let handoff = RealtimeAudioHandoff(
            source: .mic,
            slotCount: 2,
            onBuffer: { buffer in
                let marker = Int(buffer.samples[0])
                events.withLock { $0.append("pcm:\(marker)") }
                if marker == 1 {
                    enteredConsumer.fulfill()
                    releaseConsumer.wait()
                }
            },
            onOverflow: { _, frames in
                events.withLock { $0.append("gap:\(frames)") }
                overflowReported.fulfill()
            },
            beforeAdmissionCommit: {
                let admission = admissionCount.withLock { count in
                    count += 1
                    return count
                }
                if admission == 2 {
                    enteredRacingCommit.fulfill()
                    releaseRacingCommit.wait()
                }
            }
        )
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 1,
            interleaved: false
        ))
        func buffer(_ marker: Float) throws -> AVAudioPCMBuffer {
            let value = try XCTUnwrap(AVAudioPCMBuffer(
                pcmFormat: format,
                frameCapacity: 2
            ))
            value.frameLength = 2
            value.floatChannelData?[0][0] = marker
            value.floatChannelData?[0][1] = marker
            return value
        }

        try handoff.enqueue(buffer(1), muted: false)
        wait(for: [enteredConsumer], timeout: 2)
        let racingBuffer = try UncheckedAudioBuffer(value: buffer(2))
        DispatchQueue.global(qos: .userInitiated).async {
            handoff.enqueue(racingBuffer.value, muted: false)
            racingCommitReturned.fulfill()
        }
        wait(for: [enteredRacingCommit], timeout: 2)

        // Three externally reported frames close the gate. The producer
        // already copying marker 2 must then join that gap (2 frames), as
        // must marker 3 offered while the gap is pending (2 more frames).
        handoff.reportOverflow(frames: 3)
        try handoff.enqueue(buffer(3), muted: false)
        releaseRacingCommit.signal()
        wait(for: [racingCommitReturned], timeout: 2)
        releaseConsumer.signal()
        wait(for: [overflowReported], timeout: 2)
        handoff.drainSynchronously()
        try handoff.enqueue(buffer(4), muted: false)
        handoff.drainSynchronously()

        XCTAssertEqual(events.withLock { $0 }, ["pcm:1", "gap:7", "pcm:4"])
    }

    func testRawInterleavedFloat32AudioBufferListHandoff() throws {
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 2,
            interleaved: true
        ))
        var input: [Float] = [1, 10, 2, 20, 3, 30]
        let delivered = OSAllocatedUnfairLock<[Float]?>(initialState: nil)
        let handoff = RealtimeAudioHandoff(
            source: .system,
            onBuffer: { buffer in delivered.withLock { $0 = buffer.samples } },
            onOverflow: { _, _ in XCTFail("valid interleaved input overflowed") }
        )
        let list = allocateAudioBufferList(bufferCount: 1)
        defer { deallocateAudioBufferList(list) }

        input.withUnsafeMutableBytes { bytes in
            let buffers = UnsafeMutableAudioBufferListPointer(list)
            buffers[0] = AudioBuffer(
                mNumberChannels: 2,
                mDataByteSize: UInt32(bytes.count),
                mData: bytes.baseAddress
            )
            handoff.enqueue(bufferList: UnsafePointer(list), format: format)
        }
        handoff.drainSynchronously()

        XCTAssertEqual(delivered.withLock { $0 }, input)
    }

    func testRawPlanarFloat32AudioBufferListHandoffInterleavesChannels() throws {
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 2,
            interleaved: false
        ))
        var left: [Float] = [1, 2, 3]
        var right: [Float] = [10, 20, 30]
        let delivered = OSAllocatedUnfairLock<[Float]?>(initialState: nil)
        let handoff = RealtimeAudioHandoff(
            source: .system,
            onBuffer: { buffer in delivered.withLock { $0 = buffer.samples } },
            onOverflow: { _, _ in XCTFail("valid planar input overflowed") }
        )
        let list = allocateAudioBufferList(bufferCount: 2)
        defer { deallocateAudioBufferList(list) }

        left.withUnsafeMutableBytes { leftBytes in
            right.withUnsafeMutableBytes { rightBytes in
                let buffers = UnsafeMutableAudioBufferListPointer(list)
                buffers[0] = AudioBuffer(
                    mNumberChannels: 1,
                    mDataByteSize: UInt32(leftBytes.count),
                    mData: leftBytes.baseAddress
                )
                buffers[1] = AudioBuffer(
                    mNumberChannels: 1,
                    mDataByteSize: UInt32(rightBytes.count),
                    mData: rightBytes.baseAddress
                )
                handoff.enqueue(bufferList: UnsafePointer(list), format: format)
            }
        }
        handoff.drainSynchronously()

        XCTAssertEqual(delivered.withLock { $0 }, [1, 10, 2, 20, 3, 30])
    }

    func testRawMalformedAudioBufferListIsRejectedBeforeTypedRead() throws {
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 2,
            interleaved: false
        ))
        var left: [Float] = [1, 2, 3]
        var right: [Float] = [10, 20, 30]
        let delivered = OSAllocatedUnfairLock(initialState: false)
        let overflow = OSAllocatedUnfairLock<UInt64>(initialState: 0)
        let handoff = RealtimeAudioHandoff(
            source: .system,
            onBuffer: { _ in delivered.withLock { $0 = true } },
            onOverflow: { _, frames in overflow.withLock { $0 += frames } }
        )
        let list = allocateAudioBufferList(bufferCount: 2)
        defer { deallocateAudioBufferList(list) }

        left.withUnsafeMutableBytes { leftBytes in
            right.withUnsafeMutableBytes { rightBytes in
                let buffers = UnsafeMutableAudioBufferListPointer(list)
                buffers[0] = AudioBuffer(
                    mNumberChannels: 1,
                    mDataByteSize: UInt32(leftBytes.count),
                    mData: leftBytes.baseAddress
                )
                // A shorter second plane must be rejected before either
                // pointer is rebound and read as Float.
                buffers[1] = AudioBuffer(
                    mNumberChannels: 1,
                    mDataByteSize: UInt32(rightBytes.count - MemoryLayout<Float>.stride),
                    mData: rightBytes.baseAddress
                )
                handoff.enqueue(bufferList: UnsafePointer(list), format: format)
            }
        }
        handoff.drainSynchronously()

        XCTAssertFalse(delivered.withLock { $0 })
        XCTAssertGreaterThan(overflow.withLock { $0 }, 0)
    }

    func testRawMisalignedFloat32AudioBufferListIsRejectedBeforeTypedRead() throws {
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 1,
            interleaved: true
        ))
        let byteCount = 3 * MemoryLayout<Float>.stride
        let storage = UnsafeMutableRawPointer.allocate(
            byteCount: byteCount + 1,
            alignment: MemoryLayout<Float>.alignment
        )
        defer { storage.deallocate() }
        let misaligned = storage.advanced(by: 1)
        misaligned.initializeMemory(
            as: UInt8.self,
            repeating: 0,
            count: byteCount
        )
        let delivered = OSAllocatedUnfairLock(initialState: false)
        let overflow = expectation(description: "misaligned input rejected")
        let handoff = RealtimeAudioHandoff(
            source: .system,
            onBuffer: { _ in delivered.withLock { $0 = true } },
            onOverflow: { _, _ in overflow.fulfill() }
        )
        let list = allocateAudioBufferList(bufferCount: 1)
        defer { deallocateAudioBufferList(list) }
        let buffers = UnsafeMutableAudioBufferListPointer(list)
        buffers[0] = AudioBuffer(
            mNumberChannels: 1,
            mDataByteSize: UInt32(byteCount),
            mData: misaligned
        )

        handoff.enqueue(bufferList: UnsafePointer(list), format: format)
        wait(for: [overflow], timeout: 2)
        handoff.drainSynchronously()

        XCTAssertFalse(delivered.withLock { $0 })
    }

    func testAnalyzerFormatAndConverterFailuresHonorPermissiveAndStrictPolicy() {
        let failures: [PoCError] = [.noCompatibleFormat, .converterCreationFailed]
        for failure in failures {
            guard case .recordOnly(let message) = analyzerSetupDisposition(
                allowRecordOnly: true,
                error: failure
            ) else {
                XCTFail("permissive policy did not preserve recording for \(failure)")
                continue
            }
            XCTAssertTrue(message.contains("SpeechAnalyzer initialization failed"))
            XCTAssertEqual(
                analyzerSetupDisposition(allowRecordOnly: false, error: failure),
                .terminal
            )
        }
    }

    func testAnalyzerTimelinePreservesDroppedIntervalsInLaterResults() {
        var timeline = AnalyzerTimeline()
        timeline.accept(duration: 1.0)
        timeline.drop(duration: 0.25)
        timeline.accept(duration: 0.5)
        timeline.drop(duration: 0.125)

        XCTAssertEqual(timeline.map(0.75), 0.75, accuracy: 0.000_001)
        XCTAssertEqual(timeline.map(1.0), 1.25, accuracy: 0.000_001)
        XCTAssertEqual(timeline.map(1.5), 1.875, accuracy: 0.000_001)
    }

    func testAnalyzerTimelineFoldsAlternatingFinalizedGapsIntoBoundedHistory() {
        var timeline = AnalyzerTimeline()
        var compressed = 0.0
        var dropped = 0.0

        for _ in 0 ..< 20000 {
            timeline.accept(duration: 0.01)
            compressed += 0.01
            timeline.drop(duration: 0.005)
            dropped += 0.005
            XCTAssertEqual(
                timeline.map(compressed),
                compressed + dropped,
                accuracy: 0.000_001
            )
            timeline.finalize(through: compressed)
            XCTAssertEqual(timeline.retainedGapCount, 0)
        }

        timeline.accept(duration: 0.25)
        compressed += 0.25
        XCTAssertEqual(
            timeline.map(compressed),
            compressed + dropped,
            accuracy: 0.000_001
        )
        XCTAssertEqual(timeline.retainedGapCount, 0)
    }

    func testAnalyzerTimelineCoalescesDropsAtTheSameCompressedPosition() {
        var timeline = AnalyzerTimeline()
        timeline.accept(duration: 1)
        timeline.drop(duration: 0.25)
        timeline.drop(duration: 0.125)

        XCTAssertEqual(timeline.retainedGapCount, 1)
        XCTAssertEqual(timeline.map(1), 1.375, accuracy: 0.000_001)
    }

    func testAnalyzerTimelineBoundsUnfinalizedGapHistoryWithoutApproximation() {
        var timeline = AnalyzerTimeline()
        var compressed = 0.0
        var dropped = 0.0

        for index in 0 ..< AnalyzerTimeline.maximumRetainedGapCount {
            timeline.accept(duration: 0.01)
            compressed += 0.01
            XCTAssertTrue(timeline.drop(duration: 0.005))
            dropped += 0.005
            if index.isMultiple(of: 257) {
                XCTAssertEqual(
                    timeline.map(compressed),
                    compressed + dropped,
                    accuracy: 0.000_001
                )
            }
        }

        XCTAssertEqual(
            timeline.retainedGapCount,
            AnalyzerTimeline.maximumRetainedGapCount
        )
        XCTAssertEqual(
            timeline.map(compressed),
            compressed + dropped,
            accuracy: 0.000_001
        )

        // Coalescing at the last boundary remains exact and does not consume
        // capacity, but the next distinct unfinalized boundary is rejected so
        // the coordinator can disable analysis before timestamps diverge.
        XCTAssertTrue(timeline.drop(duration: 0.0025))
        dropped += 0.0025
        timeline.accept(duration: 0.01)
        compressed += 0.01
        XCTAssertFalse(timeline.drop(duration: 0.005))
        XCTAssertEqual(
            timeline.retainedGapCount,
            AnalyzerTimeline.maximumRetainedGapCount
        )
        XCTAssertEqual(
            timeline.map(compressed),
            compressed + dropped,
            accuracy: 0.000_001
        )
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

    func testNewestRecorderQueueReportsEvictedBufferFrameCount() throws {
        let format = try XCTUnwrap(AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48000,
            channels: 1,
            interleaved: false
        ))
        let evicted = try XCTUnwrap(AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: 2
        ))
        evicted.frameLength = 2
        let offered = try XCTUnwrap(AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: 7
        ))
        offered.frameLength = 7
        let (_, continuation) = AsyncStream<AVAudioPCMBuffer>.makeStream(
            bufferingPolicy: .bufferingNewest(1)
        )
        _ = continuation.yield(evicted)
        let disposition = continuation.yield(offered)
        continuation.finish()

        XCTAssertEqual(
            recorderDroppedFrameCount(disposition, offeredFrames: 7),
            2
        )
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

    func testReentrantSynchronousBridgeMutationsReturnErrorsInsteadOfDeadlocking() throws {
        let outputDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: outputDir) }
        let callbacks = CallbackCoordinator()
        let session = try WispSession(outputDir: outputDir, onResult: { _ in })
        let handle = SessionHandle(session: session, callbacks: callbacks)
        let rawPointer = Unmanaged.passRetained(handle).toOpaque()
        let pointerBits = UInt(bitPattern: rawPointer)
        let returned = expectation(description: "reentrant mutations returned")
        let results = OSAllocatedUnfairLock<[Int32]>(initialState: [])

        DispatchQueue.global(qos: .userInitiated).async {
            callbacks.invoke {
                let pointer = OpaquePointer(bitPattern: pointerBits)
                var sample: Float = 0.25
                let push = withUnsafePointer(to: &sample) {
                    wisp_session_push_transcriber_audio(
                        session: pointer,
                        source: WispSession.Source.mic.rawValue,
                        sample_rate: 48000,
                        channels: 1,
                        samples: $0,
                        sample_count: 1
                    )
                }
                let disable = wisp_session_disable_transcription(session: pointer)
                let stopCapture = wisp_session_stop_capture(session: pointer)
                let finish = wisp_session_finish_transcription(session: pointer)
                results.withLock { $0 = [push, disable, stopCapture, finish] }
                returned.fulfill()
            }
        }

        let result = XCTWaiter.wait(for: [returned], timeout: 2)
        guard result == .completed else {
            XCTFail("synchronous bridge mutation deadlocked in its own callback")
            return
        }
        XCTAssertEqual(results.withLock { $0 }, [2, 2, 2, 2])
        let error = try XCTUnwrap(wisp_session_last_error_message(
            session: OpaquePointer(bitPattern: pointerBits)
        ))
        XCTAssertTrue(String(cString: error).contains("cannot be called synchronously"))
        wisp_session_free(session: OpaquePointer(bitPattern: pointerBits))
    }

    func testInvalidTranscriberAudioArgumentsReplaceStaleLastError() throws {
        let outputDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: outputDir) }
        let callbacks = CallbackCoordinator()
        let session = try WispSession(outputDir: outputDir, onResult: { _ in })
        let handle = SessionHandle(session: session, callbacks: callbacks)
        let rawPointer = Unmanaged.passRetained(handle).toOpaque()
        let pointer = OpaquePointer(rawPointer)
        defer { wisp_session_free(session: pointer) }
        var sample: Float = 0.25

        func assertInvalid(
            expectedMessage: String,
            _ operation: (UnsafePointer<Float>) -> Int32
        ) {
            handle.setError("stale error which must be replaced")
            let result = withUnsafePointer(to: &sample, operation)
            XCTAssertEqual(result, -1)
            let error = wisp_session_last_error_message(session: pointer)
            XCTAssertEqual(error.map { String(cString: $0) }, expectedMessage)
        }

        assertInvalid(
            expectedMessage: "wisp_session_push_transcriber_audio: invalid source"
        ) { samples in
            wisp_session_push_transcriber_audio(
                session: pointer,
                source: 99,
                sample_rate: 48000,
                channels: 1,
                samples: samples,
                sample_count: 1
            )
        }
        assertInvalid(
            expectedMessage: "wisp_session_push_transcriber_audio: sample_rate must be positive"
        ) { samples in
            wisp_session_push_transcriber_audio(
                session: pointer,
                source: WispSession.Source.mic.rawValue,
                sample_rate: 0,
                channels: 1,
                samples: samples,
                sample_count: 1
            )
        }
        assertInvalid(
            expectedMessage: "wisp_session_push_transcriber_audio: channels must be positive"
        ) { samples in
            wisp_session_push_transcriber_audio(
                session: pointer,
                source: WispSession.Source.mic.rawValue,
                sample_rate: 48000,
                channels: 0,
                samples: samples,
                sample_count: 1
            )
        }
        assertInvalid(
            expectedMessage: "wisp_session_push_transcriber_audio: sample_count must be positive"
        ) { samples in
            wisp_session_push_transcriber_audio(
                session: pointer,
                source: WispSession.Source.mic.rawValue,
                sample_rate: 48000,
                channels: 1,
                samples: samples,
                sample_count: 0
            )
        }
        assertInvalid(
            expectedMessage:
            "wisp_session_push_transcriber_audio: sample_count must contain complete frames"
        ) { samples in
            wisp_session_push_transcriber_audio(
                session: pointer,
                source: WispSession.Source.mic.rawValue,
                sample_rate: 48000,
                channels: 2,
                samples: samples,
                sample_count: 1
            )
        }

        handle.setError("stale error which must be replaced")
        let nilSamples = wisp_session_push_transcriber_audio(
            session: pointer,
            source: WispSession.Source.mic.rawValue,
            sample_rate: 48000,
            channels: 1,
            samples: nil,
            sample_count: 1
        )
        XCTAssertEqual(nilSamples, -1)
        let nilError = try XCTUnwrap(wisp_session_last_error_message(session: pointer))
        XCTAssertEqual(
            String(cString: nilError),
            "wisp_session_push_transcriber_audio: samples must not be NULL"
        )
    }

    func testTranscriberAudioABIRejectsInactiveLifecycleStates() async throws {
        func invoke(
            _ session: WispSession
        ) -> (result: Int32, error: String?) {
            let callbacks = CallbackCoordinator()
            let handle = SessionHandle(session: session, callbacks: callbacks)
            let pointer = OpaquePointer(Unmanaged.passRetained(handle).toOpaque())
            defer { wisp_session_free(session: pointer) }
            var sample: Float = 0.25
            let result = withUnsafePointer(to: &sample) {
                wisp_session_push_transcriber_audio(
                    session: pointer,
                    source: WispSession.Source.mic.rawValue,
                    sample_rate: 48000,
                    channels: 1,
                    samples: $0,
                    sample_count: 1
                )
            }
            let error = wisp_session_last_error_message(session: pointer)
                .map { String(cString: $0) }
            return (result, error)
        }

        let beforeStartDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: beforeStartDir) }
        let beforeStart = try WispSession(
            outputDir: beforeStartDir,
            transcriptionEnabled: true,
            onResult: { _ in }
        )
        let beforeStartResult = invoke(beforeStart)
        XCTAssertNotEqual(beforeStartResult.result, 0)
        XCTAssertEqual(
            beforeStartResult.error,
            "Invalid session lifecycle: transcription has not started"
        )

        let recordOnlyDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: recordOnlyDir) }
        let recordOnly = try WispSession(
            outputDir: recordOnlyDir,
            transcriptionEnabled: false,
            onResult: { _ in }
        )
        let recordOnlyResult = invoke(recordOnly)
        XCTAssertNotEqual(recordOnlyResult.result, 0)
        XCTAssertEqual(
            recordOnlyResult.error,
            "Invalid session lifecycle: transcription is disabled by policy"
        )

        let afterFinishDir = makeTemporaryDirectory()
        defer { try? FileManager.default.removeItem(at: afterFinishDir) }
        let afterFinish = try WispSession(
            outputDir: afterFinishDir,
            transcriptionEnabled: true,
            onResult: { _ in }
        )
        try await afterFinish.finishTranscription()
        let afterFinishResult = invoke(afterFinish)
        XCTAssertNotEqual(afterFinishResult.result, 0)
        XCTAssertEqual(
            afterFinishResult.error,
            "Invalid session lifecycle: transcription has already stopped"
        )
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

private struct UncheckedAudioBuffer: @unchecked Sendable {
    let value: AVAudioPCMBuffer
}

private func allocateAudioBufferList(
    bufferCount: Int
) -> UnsafeMutablePointer<AudioBufferList> {
    precondition(bufferCount > 0)
    let byteCount = MemoryLayout<AudioBufferList>.size
        + (bufferCount - 1) * MemoryLayout<AudioBuffer>.stride
    let storage = UnsafeMutableRawPointer.allocate(
        byteCount: byteCount,
        alignment: MemoryLayout<AudioBufferList>.alignment
    )
    let list = storage.bindMemory(to: AudioBufferList.self, capacity: 1)
    list.initialize(to: AudioBufferList(
        mNumberBuffers: UInt32(bufferCount),
        mBuffers: AudioBuffer(mNumberChannels: 0, mDataByteSize: 0, mData: nil)
    ))
    return list
}

private func deallocateAudioBufferList(_ list: UnsafeMutablePointer<AudioBufferList>) {
    list.deinitialize(count: 1)
    UnsafeMutableRawPointer(list).deallocate()
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
