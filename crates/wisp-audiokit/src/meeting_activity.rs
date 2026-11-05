//! Which meeting tools currently hold the microphone.
//!
//! On macOS this reads Core Audio's process objects. `IsRunningInput` is the
//! same signal as the microphone indicator in Control Center, so nothing is
//! recorded or inspected beyond the app's bundle identifier.

use wisp_core::MeetingTool;

/// Why microphone activity could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MeetingActivityError {
    #[error("CoreAudio property {property} failed with status {status}")]
    CoreAudio { property: &'static str, status: i32 },
    #[error("CoreAudio returned a malformed {property} value")]
    Malformed { property: &'static str },
}

/// The meeting tools currently using an input device.
///
/// An app that opens the microphone for a voice note appears here too; the
/// detector's delay is what separates it from a call.
///
/// # Errors
/// Returns [`MeetingActivityError`] when the process list cannot be read.
/// Callers must treat that as "unknown", never as "nothing is recording".
pub fn active_meeting_tools() -> Result<Vec<MeetingTool>, MeetingActivityError> {
    platform::active_meeting_tools()
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{CStr, c_void};
    use std::mem::{MaybeUninit, size_of};
    use std::os::raw::c_char;
    use std::ptr;

    use wisp_core::MeetingTool;

    use super::MeetingActivityError;

    type OSStatus = i32;
    type AudioObjectId = u32;

    /// `kAudioObjectSystemObject`.
    const SYSTEM_OBJECT: AudioObjectId = 1;
    /// `kAudioObjectPropertyScopeGlobal`.
    const SCOPE_GLOBAL: u32 = 0x676c_6f62;
    const ELEMENT_MAIN: u32 = 0;
    /// `kAudioHardwarePropertyProcessObjectList`.
    const PROCESS_OBJECT_LIST: u32 = 0x7072_7323;
    /// `kAudioProcessPropertyBundleID`.
    const PROCESS_BUNDLE_ID: u32 = 0x7062_6964;
    /// `kAudioProcessPropertyIsRunningInput`.
    const PROCESS_IS_RUNNING_INPUT: u32 = 0x7069_7269;
    /// `kCFStringEncodingUTF8`.
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: AudioObjectId,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> OSStatus;
        fn AudioObjectGetPropertyData(
            object: AudioObjectId,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            io_size: *mut u32,
            data: *mut c_void,
        ) -> OSStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetLength(string: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(
            length: isize,
            encoding: u32,
        ) -> isize;
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(value: *const c_void);
    }

    const fn address(selector: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            selector,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        }
    }

    fn property_value<T: Copy>(
        object: AudioObjectId,
        selector: u32,
        property: &'static str,
    ) -> Result<T, MeetingActivityError> {
        let address = address(selector);
        let mut value = MaybeUninit::<T>::uninit();
        let Ok(mut size) = u32::try_from(size_of::<T>()) else {
            return Err(MeetingActivityError::Malformed { property });
        };
        // SAFETY: `address` and the output buffer are valid for the call, and
        // Core Audio writes at most `size` bytes into `value`.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &raw const address,
                0,
                ptr::null(),
                &raw mut size,
                value.as_mut_ptr().cast(),
            )
        };
        if status != 0 {
            return Err(MeetingActivityError::CoreAudio { property, status });
        }
        if !usize::try_from(size).is_ok_and(|size| size == size_of::<T>()) {
            return Err(MeetingActivityError::Malformed { property });
        }
        // SAFETY: The successful call initialised the whole value.
        Ok(unsafe { value.assume_init() })
    }

    fn property_vec<T: Copy>(
        object: AudioObjectId,
        selector: u32,
        property: &'static str,
    ) -> Result<Vec<T>, MeetingActivityError> {
        let address = address(selector);
        let mut size = 0u32;
        // SAFETY: `address` is valid for the call and `size` is writable.
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                object,
                &raw const address,
                0,
                ptr::null(),
                &raw mut size,
            )
        };
        if status != 0 {
            return Err(MeetingActivityError::CoreAudio { property, status });
        }
        let Ok(size_bytes) = usize::try_from(size) else {
            return Err(MeetingActivityError::Malformed { property });
        };
        if !size_bytes.is_multiple_of(size_of::<T>()) {
            return Err(MeetingActivityError::Malformed { property });
        }
        let count = size_bytes / size_of::<T>();
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut values: Vec<MaybeUninit<T>> = Vec::with_capacity(count);
        values.resize_with(count, MaybeUninit::uninit);
        let mut written = size;
        // SAFETY: The buffer holds `count` slots and Core Audio reports how
        // many bytes it filled in `written`.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &raw const address,
                0,
                ptr::null(),
                &raw mut written,
                values.as_mut_ptr().cast(),
            )
        };
        if status != 0 {
            return Err(MeetingActivityError::CoreAudio { property, status });
        }
        let Ok(written_bytes) = usize::try_from(written) else {
            return Err(MeetingActivityError::Malformed { property });
        };
        let filled = (written_bytes / size_of::<T>()).min(count);
        values.truncate(filled);
        // SAFETY: The successful call initialised exactly `filled` entries.
        Ok(values
            .into_iter()
            .map(|value| unsafe { value.assume_init() })
            .collect())
    }

    fn property_string(
        object: AudioObjectId,
        selector: u32,
    ) -> Option<String> {
        let raw: *const c_void = property_value(object, selector, "bundle id").ok()?;
        if raw.is_null() {
            return None;
        }
        let value = cf_string_to_string(raw);
        // SAFETY: Core Audio returns the property's CFString at +1, so this
        // call owns the reference.
        unsafe { CFRelease(raw) };
        value
    }

    fn cf_string_to_string(raw: *const c_void) -> Option<String> {
        // SAFETY: `raw` is a live CFString from the property read.
        let length = unsafe { CFStringGetLength(raw) };
        if length <= 0 {
            return None;
        }
        // SAFETY: A pure size query for a live string.
        let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) };
        if maximum <= 0 {
            return None;
        }
        let capacity = usize::try_from(maximum).ok()? + 1;
        let mut buffer = vec![0i8; capacity];
        // SAFETY: The buffer has room for the maximum byte count plus a NUL.
        let converted = unsafe {
            CFStringGetCString(
                raw,
                buffer.as_mut_ptr(),
                isize::try_from(capacity).ok()?,
                CF_STRING_ENCODING_UTF8,
            )
        };
        if converted == 0 {
            return None;
        }
        // SAFETY: A successful conversion NUL-terminates the buffer.
        Some(
            unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub(super) fn active_meeting_tools() -> Result<Vec<MeetingTool>, MeetingActivityError> {
        let process_objects =
            property_vec::<AudioObjectId>(SYSTEM_OBJECT, PROCESS_OBJECT_LIST, "process objects")?;
        let total = process_objects.len();
        let mut failed = 0usize;
        let mut tools = Vec::new();
        for process in process_objects {
            // A process that disappears between the list and this read is
            // normal churn, not a sampler failure.
            let Ok(running) =
                property_value::<u32>(process, PROCESS_IS_RUNNING_INPUT, "running input")
            else {
                failed += 1;
                continue;
            };
            if running == 0 {
                continue;
            }
            let Some(bundle_id) = property_string(process, PROCESS_BUNDLE_ID) else {
                continue;
            };
            let Some(tool) = MeetingTool::from_bundle_id(&bundle_id) else {
                continue;
            };
            if !tools.contains(&tool) {
                tools.push(tool);
            }
        }
        // Every process failing is a broken sampler, not a quiet microphone.
        if total > 0 && failed == total {
            return Err(MeetingActivityError::Malformed {
                property: "running input",
            });
        }
        Ok(tools)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use wisp_core::MeetingTool;

    use super::MeetingActivityError;

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn active_meeting_tools() -> Result<Vec<MeetingTool>, MeetingActivityError> {
        Ok(Vec::new())
    }
}
