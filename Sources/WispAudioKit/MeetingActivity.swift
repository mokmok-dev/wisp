import CoreAudio
import Foundation
import WispCore

/// Why microphone activity could not be read.
public enum MeetingActivityError: Error, CustomStringConvertible, Equatable {
  /// A Core Audio property read failed.
  case coreAudio(property: String, status: OSStatus)
  /// Core Audio returned a value of the wrong shape.
  case malformed(property: String)

  public var description: String {
    switch self {
    case .coreAudio(let property, let status):
      "CoreAudio property \(property) failed with status \(status)"
    case .malformed(let property):
      "CoreAudio returned a malformed \(property) value"
    }
  }
}

/// Which meeting tools currently hold the microphone.
///
/// Reads Core Audio's process objects. `kAudioProcessPropertyIsRunningInput` is
/// the same signal as the microphone indicator in Control Center, so nothing is
/// recorded or inspected beyond the app's bundle identifier.
///
/// An app that opens the microphone for a voice note appears here too; the
/// detector's delay is what separates it from a call.
///
/// Throws when the process list cannot be read. Callers must treat that as
/// "unknown", never as "nothing is recording".
public func activeMeetingTools() throws -> [MeetingTool] {
  let processes: [AudioObjectID] = try propertyVector(
    from: AudioObjectID(kAudioObjectSystemObject),
    selector: kAudioHardwarePropertyProcessObjectList,
    property: "process objects"
  )

  var failures = 0
  var tools: [MeetingTool] = []
  for process in processes {
    // A process that disappears between the list and this read is normal
    // churn, not a sampler failure.
    let running: UInt32
    do {
      running = try propertyValue(
        from: process,
        selector: kAudioProcessPropertyIsRunningInput,
        property: "running input"
      )
    } catch {
      failures += 1
      continue
    }
    guard running != 0,
      let bundleID = bundleIdentifier(of: process),
      let tool = MeetingTool.fromBundleID(bundleID)
    else {
      continue
    }
    if !tools.contains(tool) {
      tools.append(tool)
    }
  }
  // Every process failing is a broken sampler, not a quiet microphone.
  if !processes.isEmpty, failures == processes.count {
    throw MeetingActivityError.malformed(property: "running input")
  }
  return tools
}

private func address(_ selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
  AudioObjectPropertyAddress(
    mSelector: selector,
    mScope: kAudioObjectPropertyScopeGlobal,
    mElement: kAudioObjectPropertyElementMain
  )
}

private func propertyValue<T>(
  from object: AudioObjectID,
  selector: AudioObjectPropertySelector,
  property: String
) throws -> T {
  var address = address(selector)
  var size = UInt32(MemoryLayout<T>.size)
  let value = UnsafeMutablePointer<T>.allocate(capacity: 1)
  defer { value.deallocate() }
  let status = AudioObjectGetPropertyData(object, &address, 0, nil, &size, value)
  guard status == noErr else {
    throw MeetingActivityError.coreAudio(property: property, status: status)
  }
  guard size == MemoryLayout<T>.size else {
    throw MeetingActivityError.malformed(property: property)
  }
  return value.pointee
}

private func propertyVector<T>(
  from object: AudioObjectID,
  selector: AudioObjectPropertySelector,
  property: String
) throws -> [T] {
  var address = address(selector)
  var size: UInt32 = 0
  var status = AudioObjectGetPropertyDataSize(object, &address, 0, nil, &size)
  guard status == noErr else {
    throw MeetingActivityError.coreAudio(property: property, status: status)
  }
  let stride = MemoryLayout<T>.stride
  guard size % UInt32(stride) == 0 else {
    throw MeetingActivityError.malformed(property: property)
  }
  let count = Int(size) / stride
  guard count > 0 else { return [] }

  let buffer = UnsafeMutablePointer<T>.allocate(capacity: count)
  defer { buffer.deallocate() }
  var written = size
  status = AudioObjectGetPropertyData(object, &address, 0, nil, &written, buffer)
  guard status == noErr else {
    throw MeetingActivityError.coreAudio(property: property, status: status)
  }
  return Array(
    UnsafeBufferPointer(start: buffer, count: min(count, Int(written) / stride))
  )
}

/// The process's bundle identifier, or `nil` when Core Audio reports none.
private func bundleIdentifier(of process: AudioObjectID) -> String? {
  var address = address(kAudioProcessPropertyBundleID)
  var value: Unmanaged<CFString>?
  var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
  let status = withUnsafeMutablePointer(to: &value) { pointer in
    AudioObjectGetPropertyData(process, &address, 0, nil, &size, pointer)
  }
  guard status == noErr, let identifier = value?.takeRetainedValue() else {
    return nil
  }
  return identifier as String
}
