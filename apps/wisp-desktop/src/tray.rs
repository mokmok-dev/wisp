//! macOS menu bar extra (status item): a small Wisp icon that lives in the
//! system menu bar so the user can start/stop recording, surface the window,
//! or quit without the main window being focused.
//!
//! Built on the `tray-icon` crate (which wraps `NSStatusItem` for us, keeping
//! the unsafe Cocoa FFI out of this crate). The status item dispatches menu
//! clicks onto a process-global channel; we poll it from a GPUI foreground
//! task — the same pattern as the other `cx.spawn` pumps in `main.rs` — so
//! every handler runs on the main thread with a live `App`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AsyncApp, Entity, Timer};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::app::{AppModel, SessionState};
use crate::session_runner::SessionRunner;
use crate::toggle_recording;

/// How often we drain the tray's menu-event channel. The status item pushes
/// clicks onto a global queue; 80ms is well below the threshold where a menu
/// click would feel laggy while staying cheap when idle.
const TRAY_POLL_INTERVAL: Duration = Duration::from_millis(80);

/// Install the menu bar status item and wire its menu to the recording state
/// machine. `recordings_dir` is where a freshly started session's WAVs land,
/// mirroring the in-window Record button.
pub fn configure(
    cx: &mut App,
    runner: Arc<SessionRunner>,
    model: &Entity<AppModel>,
    recordings_dir: PathBuf,
) {
    let active = is_active(model.read(cx).state);

    let record_item = MenuItem::new(record_label(active), true, None);
    let open_item = MenuItem::new("Open Wisp", true, None);
    let quit_item = MenuItem::new("Quit Wisp", true, None);

    let menu = Menu::new();
    if let Err(err) = menu.append_items(&[
        &record_item,
        &PredefinedMenuItem::separator(),
        &open_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]) {
        eprintln!("wisp: failed to build tray menu: {err}");
        return;
    }

    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip(active))
        .with_icon(record_icon(active))
        .with_icon_as_template(true)
        .build()
    {
        Ok(tray) => tray,
        Err(err) => {
            // A missing status item is non-fatal: the in-window button and
            // the app menu still drive recording. Log and carry on.
            eprintln!("wisp: failed to create menu bar status item: {err}");
            return;
        },
    };

    // Menu ids are plain strings, so they're cheap to clone into the poll
    // loop and compare against incoming events.
    let record_id = record_item.id().clone();
    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();

    spawn_event_pump(
        cx,
        runner,
        model.clone(),
        recordings_dir,
        [record_id, open_id, quit_id],
    );
    observe_state(cx, model, tray, record_item, active);
}

/// Poll the tray's global menu-event channel and act on clicks. Holds the
/// runner/model handles so it can reuse `toggle_recording`, exactly like the
/// in-window Record button.
fn spawn_event_pump(
    cx: &mut App,
    runner: Arc<SessionRunner>,
    model: Entity<AppModel>,
    recordings_dir: PathBuf,
    [record_id, open_id, quit_id]: [tray_icon::menu::MenuId; 3],
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        let receiver = MenuEvent::receiver();
        loop {
            Timer::after(TRAY_POLL_INTERVAL).await;
            // Drain everything queued this tick; ids are all we need to act.
            let pending: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.id)
                .collect();
            if pending.is_empty() {
                continue;
            }
            let result = cx.update(|cx| {
                for id in &pending {
                    if *id == record_id {
                        toggle_recording(&runner, &model, &recordings_dir, cx);
                    } else if *id == open_id {
                        // Bring Wisp to the foreground; the main window is
                        // already open behind the scenes.
                        cx.activate(true);
                    } else if *id == quit_id {
                        // `on_app_quit` (registered in `app_menu`) drains the
                        // worker and persists segments before we exit.
                        cx.quit();
                    }
                }
            });
            if result.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Keep the status item's label, icon, and tooltip in sync with the session
/// state, and keep the `TrayIcon` alive for the lifetime of the app (the
/// detached observer owns it).
fn observe_state(
    cx: &mut App,
    model: &Entity<AppModel>,
    tray: TrayIcon,
    record_item: MenuItem,
    initial_active: bool,
) {
    let mut active = initial_active;
    cx.observe(model, move |model, cx| {
        // `tray` is moved in purely so it outlives `configure`; touch it so
        // the capture isn't flagged as unused.
        let _ = &tray;
        let now = is_active(model.read(cx).state);
        if now == active {
            return;
        }
        active = now;
        record_item.set_text(record_label(now));
        if let Err(err) = tray.set_icon(Some(record_icon(now))) {
            eprintln!("wisp: failed to update tray icon: {err}");
        }
        if let Err(err) = tray.set_tooltip(Some(tooltip(now))) {
            eprintln!("wisp: failed to update tray tooltip: {err}");
        }
    })
    .detach();
}

/// True while a session is live or mid-transition — i.e. the menu should
/// offer "Stop Recording" rather than "Start Recording".
fn is_active(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Recording { .. } | SessionState::Starting | SessionState::Stopping
    )
}

fn record_label(active: bool) -> &'static str {
    if active {
        "Stop Recording"
    } else {
        "Start Recording"
    }
}

fn tooltip(active: bool) -> &'static str {
    if active { "Wisp — Recording" } else { "Wisp" }
}

/// A 32×32 template glyph: a filled dot while recording, a hollow ring while
/// idle. Drawn as alpha-only (RGB black) so macOS tints it to match the menu
/// bar in light and dark mode via `with_icon_as_template`.
// Every cast here is a coordinate in `0..=32`, far inside f32's exact-integer
// range, so the precision-loss lint doesn't apply.
#[allow(clippy::cast_precision_loss)]
fn record_icon(active: bool) -> Icon {
    const SIZE: u32 = 32;
    let centre = (SIZE as f32 - 1.0) / 2.0;
    let outer = SIZE as f32 * 0.34;
    let inner = outer - SIZE as f32 * 0.12;

    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            let dist = dx.mul_add(dx, dy * dy).sqrt();
            let opaque = if active {
                dist <= outer
            } else {
                dist <= outer && dist >= inner
            };
            if opaque {
                let i = ((y * SIZE + x) * 4) as usize;
                // RGB stays 0 (template uses alpha only); set full alpha.
                rgba[i + 3] = 255;
            }
        }
    }

    // 32×32 RGBA is always a valid icon size; the only failure mode is a
    // length/dimension mismatch, which can't happen with this fixed buffer.
    Icon::from_rgba(rgba, SIZE, SIZE).expect("32x32 RGBA is a valid tray icon")
}
