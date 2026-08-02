@preconcurrency import AVFoundation
import AudioToolbox
import Foundation
import os.lock

/// Encodes PCM buffers as Opus on a background task and writes a single,
/// continuously playable Ogg logical stream.
///
/// Every completed Opus packet is emitted immediately as its own Ogg page.
/// This costs a small amount of container overhead, but a crash loses at most
/// the packet currently being encoded. A missing EOS flag does not invalidate
/// the pages already present in the file.
final class OpusOggRecorder: @unchecked Sendable {
  private static let queueCapacity = 256

  private let continuation: AsyncStream<AVAudioPCMBuffer>.Continuation
  private let encodingTask: Task<Void, Never>
  private let droppedFrames = OSAllocatedUnfairLock<UInt64>(initialState: 0)
  private let onDroppedFrames: @Sendable (UInt64) -> Void

  init(
    url: URL,
    sourceFormat: AVAudioFormat,
    onFatal: @escaping @Sendable (String) -> Void = { wispLog($0) },
    onDroppedFrames: @escaping @Sendable (UInt64) -> Void = { _ in }
  ) throws {
    self.onDroppedFrames = onDroppedFrames
    let channelCount = min(max(sourceFormat.channelCount, 1), 2)
    let encoder = try OpusEncoder(url: url, channelCount: channelCount)
    let (stream, continuation) = AsyncStream<AVAudioPCMBuffer>.makeStream(
      bufferingPolicy: .bufferingNewest(Self.queueCapacity)
    )
    self.continuation = continuation
    encodingTask = Task.detached(priority: .utility) {
      do {
        for await buffer in stream {
          if Task.isCancelled {
            try encoder.closeAfterError()
            return
          }
          try encoder.encode(buffer)
        }
        if Task.isCancelled {
          try encoder.closeAfterError()
          return
        }
        try encoder.finish()
      } catch {
        onFatal("[OGG] encoder error for \(url.lastPathComponent): \(error)")
        try? encoder.closeAfterError()
      }
    }
  }

  /// Copies and queues a callback-owned PCM buffer without performing codec
  /// or file I/O on the real-time audio thread.
  func push(_ buffer: AVAudioPCMBuffer) {
    let frames = UInt64(buffer.frameLength)
    guard let copy = buffer.detachedCopy() else {
      reportDropped(frames)
      return
    }
    let disposition = continuation.yield(copy)
    if let dropped = recorderDroppedFrameCount(
      disposition,
      offeredFrames: frames
    ) {
      reportDropped(dropped)
    }
  }

  /// Stops accepting PCM, drains the bounded queue, flushes the Opus
  /// converter, writes EOS, and closes the file.
  func finish() async {
    continuation.finish()
    await encodingTask.value
  }

  /// Stop without intentionally draining queued buffers. Already-written
  /// Ogg pages remain usable as a truncated recording.
  func abort() async {
    encodingTask.cancel()
    continuation.finish()
    await encodingTask.value
  }

  private func reportDropped(_ frames: UInt64) {
    guard frames > 0 else { return }
    droppedFrames.withLock { $0 &+= frames }
    onDroppedFrames(frames)
  }
}

/// AsyncStream's `bufferingNewest` evicts and returns the oldest buffered
/// element. Account for that element rather than the newly accepted one.
func recorderDroppedFrameCount(
  _ disposition: AsyncStream<AVAudioPCMBuffer>.Continuation.YieldResult,
  offeredFrames: UInt64
) -> UInt64? {
  switch disposition {
  case .enqueued:
    nil
  case .dropped(let droppedBuffer):
    UInt64(droppedBuffer.frameLength)
  case .terminated:
    offeredFrames
  @unknown default:
    offeredFrames
  }
}

private final class OpusEncoder: @unchecked Sendable {
  private static let outputSampleRate = 48000.0
  private static let bitRatePerChannel = 32000

  private let channelCount: AVAudioChannelCount
  private let outputFormat: AVAudioFormat
  private let writer: OggOpusWriter
  private var converter: AVAudioConverter?
  private var converterInputFormat: AVAudioFormat?
  private var inputSamplesAt48k = 0.0
  private var isClosed = false

  init(url: URL, channelCount: AVAudioChannelCount) throws {
    self.channelCount = channelCount
    guard
      let outputFormat = AVAudioFormat(settings: [
        AVFormatIDKey: kAudioFormatOpus,
        AVSampleRateKey: Self.outputSampleRate,
        AVNumberOfChannelsKey: channelCount,
      ])
    else {
      throw PoCError.converterCreationFailed
    }
    self.outputFormat = outputFormat

    // Apple's Opus encoder reports the codec look-ahead through
    // `primeInfo`. It is normally 312 samples at 48 kHz.
    guard
      let probe = AVAudioConverter(
        from: OpusEncoder.normalizedPCMFormat(for: channelCount),
        to: outputFormat
      )
    else {
      throw PoCError.converterCreationFailed
    }
    probe.bitRate = Self.bitRatePerChannel * Int(channelCount)
    let preSkip = UInt16(clamping: probe.primeInfo.leadingFrames)
    writer = try OggOpusWriter(
      url: url,
      channelCount: UInt8(channelCount),
      preSkip: preSkip
    )
  }

  func encode(_ buffer: AVAudioPCMBuffer) throws {
    guard !isClosed, buffer.frameLength > 0 else { return }
    inputSamplesAt48k +=
      Double(buffer.frameLength)
      * Self.outputSampleRate / buffer.format.sampleRate

    let converter = try converter(for: buffer.format)
    let input = ConverterInput(buffer)
    try drain(converter: converter) { _, status in
      input.provide(status: status)
    }
  }

  func finish() throws {
    guard !isClosed else { return }
    if let converter {
      try drain(converter: converter) { _, status in
        status.pointee = .endOfStream
        return nil
      }
    }
    let finalGranule = writer.preSkip + UInt64(inputSamplesAt48k.rounded())
    try writer.finish(finalGranule: finalGranule)
    isClosed = true
  }

  func closeAfterError() throws {
    guard !isClosed else { return }
    // Do not manufacture EOS after an encoding error. Closing here leaves
    // the already completed Ogg pages usable as a truncated recording.
    try writer.closeTruncated()
    isClosed = true
  }

  private func converter(for inputFormat: AVAudioFormat) throws -> AVAudioConverter {
    if let converter, converterInputFormat?.isEqual(inputFormat) == true {
      return converter
    }
    if let converter {
      // A device switch can change the callback's PCM format. Flush the
      // old codec before replacing it so its final buffered packet is
      // not silently lost from the continuous Ogg stream.
      try drain(converter: converter) { _, status in
        status.pointee = .endOfStream
        return nil
      }
    }
    guard let converter = AVAudioConverter(from: inputFormat, to: outputFormat) else {
      throw PoCError.converterCreationFailed
    }
    converter.bitRate = Self.bitRatePerChannel * Int(channelCount)
    self.converter = converter
    converterInputFormat = inputFormat
    return converter
  }

  private func drain(
    converter: AVAudioConverter,
    inputBlock: @escaping AVAudioConverterInputBlock
  ) throws {
    while true {
      let output = AVAudioCompressedBuffer(
        format: outputFormat,
        packetCapacity: 1,
        maximumPacketSize: converter.maximumOutputPacketSize
      )
      var error: NSError?
      let status = converter.convert(
        to: output,
        error: &error,
        withInputFrom: inputBlock
      )
      if let error { throw error }
      if output.packetCount > 0, output.byteLength > 0 {
        let packet = Data(bytes: output.data, count: Int(output.byteLength))
        try writer.writeAudioPacket(
          packet,
          sampleCount: OpusPacket.sampleCount(packet)
        )
      }
      switch status {
      case .haveData:
        continue
      case .inputRanDry, .endOfStream:
        return
      case .error:
        throw PoCError.converterCreationFailed
      @unknown default:
        return
      }
    }
  }

  private static func normalizedPCMFormat(
    for channels: AVAudioChannelCount
  ) -> AVAudioFormat {
    AVAudioFormat(
      commonFormat: .pcmFormatFloat32,
      sampleRate: outputSampleRate,
      channels: channels,
      interleaved: false
    )!
  }
}

private final class ConverterInput: @unchecked Sendable {
  private let buffer: AVAudioPCMBuffer
  private var supplied = false

  init(_ buffer: AVAudioPCMBuffer) {
    self.buffer = buffer
  }

  func provide(
    status: UnsafeMutablePointer<AVAudioConverterInputStatus>
  ) -> AVAudioBuffer? {
    if supplied {
      status.pointee = .noDataNow
      return nil
    }
    supplied = true
    status.pointee = .haveData
    return buffer
  }
}

private enum OpusPacket {
  /// Returns an Opus packet's decoded duration in the mandatory 48 kHz Ogg
  /// granule timebase (RFC 6716, section 3.1).
  static func sampleCount(_ packet: Data) -> UInt64 {
    guard let toc = packet.first else { return 0 }
    let config = toc >> 3
    let samplesPerFrame: UInt64 =
      if config >= 16 {
        120 << UInt64(config & 0x03)
      } else if config >= 12 {
        480 << UInt64(config & 0x01)
      } else if config & 0x03 == 0x03 {
        2880
      } else {
        480 << UInt64(config & 0x03)
      }

    let frameCode = toc & 0x03
    let frameCount: UInt64 =
      switch frameCode {
      case 0:
        1
      case 1, 2:
        2
      default:
        packet.count > 1 ? UInt64(packet[packet.startIndex + 1] & 0x3F) : 0
      }
    return min(samplesPerFrame * frameCount, 5760)
  }
}

private final class OggOpusWriter {
  let preSkip: UInt64

  private let file: FileHandle
  private let serialNumber: UInt32
  private var sequenceNumber: UInt32 = 0
  private var encodedGranule: UInt64 = 0
  private var bytesWritten: UInt64 = 0
  private var audioPagesSinceSync = 0
  private var lastAudioPage:
    (
      offset: UInt64,
      sequence: UInt32,
      packet: Data,
      startGranule: UInt64,
      endGranule: UInt64
    )?
  private var isClosed = false

  init(url: URL, channelCount: UInt8, preSkip: UInt16) throws {
    guard FileManager.default.createFile(atPath: url.path, contents: nil),
      let file = FileHandle(forWritingAtPath: url.path)
    else {
      throw CocoaError(.fileWriteUnknown)
    }
    self.file = file
    serialNumber = UInt32.random(in: UInt32.min...UInt32.max)
    self.preSkip = UInt64(preSkip)

    var head = Data("OpusHead".utf8)
    head.append(1)  // version
    head.append(channelCount)
    head.appendLittleEndian(preSkip)
    head.appendLittleEndian(UInt32(48000))
    head.appendLittleEndian(Int16(0))  // output gain
    head.append(0)  // channel mapping family 0 (mono/stereo)
    try writePage(packet: head, granule: 0, flags: 0x02)  // BOS

    let vendor = Data("WispAudioKit".utf8)
    var tags = Data("OpusTags".utf8)
    tags.appendLittleEndian(UInt32(vendor.count))
    tags.append(vendor)
    tags.appendLittleEndian(UInt32(0))  // user comment count
    try writePage(packet: tags, granule: 0, flags: 0)
    try file.synchronize()
  }

  func writeAudioPacket(_ packet: Data, sampleCount: UInt64) throws {
    guard !isClosed, !packet.isEmpty else { return }
    let startGranule = encodedGranule
    encodedGranule += sampleCount
    let page = try writePage(packet: packet, granule: encodedGranule, flags: 0)
    lastAudioPage = (
      offset: page.offset,
      sequence: page.sequence,
      packet: packet,
      startGranule: startGranule,
      endGranule: encodedGranule
    )
    audioPagesSinceSync += 1
    // Opus normally produces 20 ms packets. Sync about once per second so
    // even a machine-level crash has a bounded durability window; this
    // runs on the utility encoder task, never the audio callback.
    if audioPagesSinceSync >= 50 {
      try file.synchronize()
      audioPagesSinceSync = 0
    }
  }

  func finish(finalGranule: UInt64) throws {
    guard !isClosed else { return }
    if let lastAudioPage {
      let trimmedGranule = min(
        lastAudioPage.endGranule,
        max(lastAudioPage.startGranule, finalGranule)
      )
      let eosPage = try makePage(
        packet: lastAudioPage.packet,
        granule: trimmedGranule,
        flags: 0x04,  // EOS
        sequence: lastAudioPage.sequence
      )
      try file.seek(toOffset: lastAudioPage.offset)
      try file.write(contentsOf: eosPage)
    } else {
      _ = try writePage(packet: Data(), granule: 0, flags: 0x04)
    }
    try file.synchronize()
    try file.close()
    isClosed = true
  }

  func closeTruncated() throws {
    guard !isClosed else { return }
    // All completed packets have already been written. Leaving the final
    // page without EOS is intentional: readers can recover through it.
    try file.synchronize()
    try file.close()
    isClosed = true
  }

  @discardableResult
  private func writePage(
    packet: Data,
    granule: UInt64,
    flags: UInt8
  ) throws -> (offset: UInt64, sequence: UInt32) {
    let offset = bytesWritten
    let sequence = sequenceNumber
    let page = try makePage(
      packet: packet,
      granule: granule,
      flags: flags,
      sequence: sequence
    )
    try file.write(contentsOf: page)
    bytesWritten += UInt64(page.count)
    sequenceNumber &+= 1
    return (offset, sequence)
  }

  private func makePage(
    packet: Data,
    granule: UInt64,
    flags: UInt8,
    sequence: UInt32
  ) throws -> Data {
    let quotient = packet.count / 255
    let remainder = packet.count % 255
    let segmentCount = quotient + 1
    guard segmentCount <= 255 else {
      throw CocoaError(.fileWriteUnknown)
    }

    var page = Data()
    page.append(Data("OggS".utf8))
    page.append(0)  // stream structure version
    page.append(flags)
    page.appendLittleEndian(granule)
    page.appendLittleEndian(serialNumber)
    page.appendLittleEndian(sequence)
    page.appendLittleEndian(UInt32(0))  // checksum placeholder
    page.append(UInt8(segmentCount))
    if quotient > 0 {
      page.append(contentsOf: repeatElement(UInt8(255), count: quotient))
    }
    page.append(UInt8(remainder))
    page.append(packet)

    let checksum = page.oggCRC
    page.replaceSubrange(22..<26, with: checksum.littleEndianBytes)
    return page
  }
}

extension AVAudioPCMBuffer {
  fileprivate func detachedCopy() -> AVAudioPCMBuffer? {
    guard
      let copy = AVAudioPCMBuffer(
        pcmFormat: format,
        frameCapacity: frameLength
      )
    else { return nil }
    copy.frameLength = frameLength

    let source = audioBufferList.pointee
    let destination = copy.mutableAudioBufferList
    let count = min(Int(source.mNumberBuffers), Int(destination.pointee.mNumberBuffers))
    let sourceBuffers = UnsafeMutableAudioBufferListPointer(
      UnsafeMutablePointer(mutating: audioBufferList)
    )
    let destinationBuffers = UnsafeMutableAudioBufferListPointer(destination)
    for index in 0..<count {
      guard let sourceData = sourceBuffers[index].mData,
        let destinationData = destinationBuffers[index].mData
      else { continue }
      let byteCount = min(
        Int(sourceBuffers[index].mDataByteSize),
        Int(destinationBuffers[index].mDataByteSize)
      )
      memcpy(destinationData, sourceData, byteCount)
      destinationBuffers[index].mDataByteSize = UInt32(byteCount)
    }
    return copy
  }
}

extension Data {
  fileprivate mutating func appendLittleEndian(_ value: some FixedWidthInteger) {
    append(contentsOf: value.littleEndianBytes)
  }

  fileprivate var oggCRC: UInt32 {
    var crc: UInt32 = 0
    for byte in self {
      crc ^= UInt32(byte) << 24
      for _ in 0..<8 {
        crc =
          (crc & 0x8000_0000) != 0
          ? (crc << 1) ^ 0x04C1_1DB7
          : crc << 1
      }
    }
    return crc
  }
}

extension FixedWidthInteger {
  fileprivate var littleEndianBytes: [UInt8] {
    withUnsafeBytes(of: littleEndian) { Array($0) }
  }
}
