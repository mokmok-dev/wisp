import AppKit
import Carbon.HIToolbox
import WispCore

/// Why `RegisterEventHotKey` refused a chord.
struct GlobalShortcutFailure: Error, Equatable, Sendable {
  /// `eventHotKeyExistsErr`: another process already owns this chord
  /// exclusively.
  let takenByAnotherApplication: Bool
  let status: Int32
  var message: String
}

/// Carbon registers application-target hot keys that fire no matter which
/// app is frontmost; there is no modern replacement, which is why this
/// wrapper exists around `RegisterEventHotKey` alone.
@MainActor
final class GlobalShortcut {
  private var handlerRef: EventHandlerRef?
  private var hotKeyRef: EventRef?
  /// Kept so a failed re-registration can restore exactly what the user had.
  private var registeredChord: DictationChord?

  private let onPress: @MainActor () -> Void

  init(onPress: @escaping @MainActor () -> Void) {
    self.onPress = onPress
  }

  /// Replace the registration, restoring the previous chord when the new one
  /// is rejected, so a failed change never leaves dictation with no chord.
  func register(_ chord: DictationChord) -> GlobalShortcutFailure? {
    let previous = registeredChord
    unregisterHotKey()
    let status = install(chord)
    if status == noErr {
      return nil
    }
    if let previous {
      _ = install(previous)
    }
    return failure(for: status)
  }

  func unregister() {
    registeredChord = nil
    unregisterHotKey()
  }

  // MARK: - Registration

  private func install(_ chord: DictationChord) -> OSStatus {
    if handlerRef == nil {
      let installed = installHandler()
      if installed != noErr {
        return installed
      }
    }
    var hotKey: EventRef?
    let hotKeyID = EventHotKeyID(
      // Wisp's hot key namespace, bytes for "WSPD".
      signature: OSType(0x5753_5044),
      id: 1
    )
    let status = RegisterEventHotKey(
      UInt32(chord.keyCode),
      Self.carbonModifiers(chord.modifiers),
      hotKeyID,
      GetApplicationEventTarget(),
      0,
      &hotKey
    )
    if status == noErr {
      hotKeyRef = hotKey
      registeredChord = chord
    }
    return status
  }

  private func installHandler() -> OSStatus {
    var eventType = EventTypeSpec(
      eventClass: OSType(kEventClassKeyboard),
      eventKind: UInt32(kEventHotKeyPressed)
    )
    var handler: EventHandlerRef?
    let status = InstallEventHandler(
      GetApplicationEventTarget(),
      { _, event, userData in
        var hotKeyID = EventHotKeyID()
        let status = GetEventParameter(
          event,
          EventParamName(kEventParamDirectObject),
          EventParamType(typeEventHotKeyID),
          nil,
          MemoryLayout<EventHotKeyID>.size,
          nil,
          &hotKeyID
        )
        guard status == noErr,
          hotKeyID.signature == OSType(0x5753_5044),
          hotKeyID.id == 1,
          let userData
        else { return noErr }
        // Carbon delivers application-target hot keys on the main run loop.
        let shortcut = Unmanaged<GlobalShortcut>
          .fromOpaque(userData)
          .takeUnretainedValue()
        MainActor.assumeIsolated {
          shortcut.onPress()
        }
        return noErr
      },
      1,
      &eventType,
      Unmanaged.passUnretained(self).toOpaque(),
      &handler
    )
    if status == noErr {
      handlerRef = handler
    }
    return status
  }

  private func unregisterHotKey() {
    if let hotKeyRef {
      UnregisterEventHotKey(hotKeyRef)
      self.hotKeyRef = nil
    }
  }

  private func failure(for status: OSStatus) -> GlobalShortcutFailure {
    let taken = status == eventHotKeyExistsErr
    let message =
      taken
      ? "This shortcut is already reserved by another application. Pick a different one in Settings — or quit that app."
      : "The shortcut could not be registered (Carbon error \(status)). Pick a different one in Settings."
    return GlobalShortcutFailure(
      takenByAnotherApplication: taken,
      status: status,
      message: message
    )
  }

  /// Carbon has its own bit values for modifier keys, so the chord's flags
  /// map here rather than leaking Carbon into `WispCore`.
  private static func carbonModifiers(_ modifiers: DictationChord.Modifiers) -> UInt32 {
    var mask: UInt32 = 0
    if modifiers.contains(.control) { mask |= UInt32(controlKey) }
    if modifiers.contains(.option) { mask |= UInt32(optionKey) }
    if modifiers.contains(.shift) { mask |= UInt32(shiftKey) }
    if modifiers.contains(.command) { mask |= UInt32(cmdKey) }
    return mask
  }
}
