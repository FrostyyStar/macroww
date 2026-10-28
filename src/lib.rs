//! Tauri layer: wires the engine (`engine.rs`, unit-tested and exercised
//! against a live X server) to the window, native file dialogs, and the
//! global hotkeys.
//!
//! Design rules that matter:
//!  * The recorded macro lives HERE, in `AppState`. The window never
//!    receives or sends event data — only a tiny `Status` snapshot — so a
//!    long recording can't bog down the UI.
//!  * Every action is one `AppState::do_*` method. Buttons (commands) and
//!    hotkeys call the same methods, so they behave identically, and every
//!    state change emits `status_changed` so the UI can never drift.
//!  * Per-input-event work is tiny. Nothing is emitted to the window per
//!    mouse move (that flooded and froze the old UI); the live event count
//!    is pushed at most 4 times a second.

mod engine;

use engine::{
    clamp_speed, finalize_recording, lock, run_playback, HotkeyAction, HotkeyTracker,
    InputSimulator, MacroFile, PlaybackControl, RecordingState,
};
use rdev::EventType;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

/// Hotkeys (global — they work while another app has focus):
///   Ctrl+Alt+R      start / stop recording
///   Ctrl+Alt+P      play / stop the current macro
///   Ctrl+Alt+Space  pause / resume playback
/// (see `HotkeyTracker` in engine.rs)

/// How playback runs. Held in the backend (not the window) so the hotkey
/// and the Play button always play the same way.
#[derive(Clone, Copy)]
struct PlaybackOptions {
    speed: f64,
    /// Number of plays; `engine::REPEAT_FOREVER` (0) loops until stopped.
    repeat: u32,
}

/// Cheap to clone; every clone shares the same underlying state.
#[derive(Clone)]
struct AppState {
    recording: RecordingState,
    playback: PlaybackControl,
    hotkeys: HotkeyTracker,
    current_macro: Arc<Mutex<Option<MacroFile>>>,
    options: Arc<Mutex<PlaybackOptions>>,
    last_status_push_ms: Arc<AtomicU64>,
}

/// Everything the window needs to draw itself. Deliberately tiny.
#[derive(Clone, Serialize)]
struct Status {
    state: &'static str, // "idle" | "recording" | "playing" | "paused"
    has_macro: bool,
    macro_name: String,
    event_count: usize,
    duration_ms: u64,
    speed: f64,
    repeat: u32,
}

#[derive(Clone, Serialize)]
struct Progress {
    done: usize,
    total: usize,
    loop_index: u32,
    loop_total: u32,
}

impl AppState {
    fn new() -> Self {
        AppState {
            recording: RecordingState::default(),
            playback: PlaybackControl::default(),
            hotkeys: HotkeyTracker::default(),
            current_macro: Arc::new(Mutex::new(None)),
            options: Arc::new(Mutex::new(PlaybackOptions { speed: 1.0, repeat: 1 })),
            last_status_push_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn is_recording(&self) -> bool {
        self.recording.is_recording.load(Ordering::SeqCst)
    }
    fn is_playing(&self) -> bool {
        self.playback.is_playing.load(Ordering::SeqCst)
    }

    fn status(&self) -> Status {
        let recording = self.is_recording();
        let playing = self.is_playing();
        let paused = self.playback.is_paused.load(Ordering::SeqCst);
        let mac = lock(&self.current_macro);
        let options = *lock(&self.options);
        let state = if recording {
            "recording"
        } else if playing && paused {
            "paused"
        } else if playing {
            "playing"
        } else {
            "idle"
        };
        Status {
            state,
            has_macro: mac.is_some(),
            macro_name: mac.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
            event_count: if recording {
                self.recording.len()
            } else {
                mac.as_ref().map(|m| m.events.len()).unwrap_or(0)
            },
            duration_ms: mac.as_ref().map(|m| m.duration_ms()).unwrap_or(0),
            speed: options.speed,
            repeat: options.repeat,
        }
    }

    fn push_status(&self, app: &AppHandle) {
        let _ = app.emit("status_changed", self.status());
    }

    fn notice(&self, app: &AppHandle, text: impl Into<String>) {
        let _ = app.emit("notice", text.into());
    }

    // ---- actions: buttons and hotkeys both land here ----

    fn do_toggle_recording(&self, app: &AppHandle, via_ui: bool) {
        if self.is_recording() {
            self.do_stop_recording(app, via_ui);
        } else {
            self.do_start_recording(app);
        }
    }

    fn do_start_recording(&self, app: &AppHandle) {
        if self.is_playing() {
            self.notice(app, "Stop playback before recording.");
            return;
        }
        self.recording.start(now_ms(SystemTime::now()));
        self.push_status(app);
    }

    /// `via_ui`: recording was stopped by clicking the app's own button, so
    /// the pointer trip to that button and the click itself are trimmed
    /// from the end of the macro.
    fn do_stop_recording(&self, app: &AppHandle, via_ui: bool) {
        let events = finalize_recording(self.recording.stop(), via_ui);
        if events.is_empty() {
            self.notice(app, "Nothing was recorded.");
        } else {
            *lock(&self.current_macro) = Some(MacroFile::new("untitled", events));
        }
        self.push_status(app);
    }

    fn do_toggle_playback(&self, app: &AppHandle) {
        if self.is_playing() {
            self.do_stop_playback(app);
            return;
        }
        let mac = lock(&self.current_macro).clone();
        match mac {
            Some(m) => self.do_start_playback(app, m),
            None => self.notice(app, "Record or open a macro first."),
        }
    }

    fn do_start_playback(&self, app: &AppHandle, macro_file: MacroFile) {
        if self.is_recording() {
            self.notice(app, "Stop recording before playing.");
            return;
        }
        if self.is_playing() {
            return;
        }
        if macro_file.events.is_empty() {
            self.notice(app, "This macro is empty.");
            return;
        }

        let options = *lock(&self.options);

        // Reset flags synchronously, BEFORE spawning the thread, so a stop
        // request that arrives right away can't be lost (see engine.rs).
        self.playback.begin();
        self.push_status(app);

        let me = self.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            let mut sim = RealSimulator;
            let mut last_push = Instant::now() - Duration::from_secs(1);
            run_playback(
                &macro_file.events,
                options.speed,
                options.repeat,
                &me.playback,
                &mut sim,
                |done, total, loop_index, loop_total| {
                    // At most ~4 updates/second, never one per event.
                    if last_push.elapsed() >= Duration::from_millis(250) {
                        last_push = Instant::now();
                        let _ = app.emit(
                            "playback_progress",
                            Progress { done, total, loop_index, loop_total },
                        );
                    }
                },
            );
            me.push_status(&app);
        });
    }

    fn do_stop_playback(&self, app: &AppHandle) {
        if !self.is_playing() {
            return;
        }
        self.playback.stop_requested.store(true, Ordering::SeqCst);
        self.playback.is_paused.store(false, Ordering::SeqCst);
        self.push_status(app);
    }

    fn do_toggle_pause(&self, app: &AppHandle) {
        if !self.is_playing() {
            return;
        }
        let now_paused = !self.playback.is_paused.load(Ordering::SeqCst);
        self.playback.is_paused.store(now_paused, Ordering::SeqCst);
        self.push_status(app);
    }

    /// Speed and repeat are read when playback starts, so they can't change
    /// mid-run; say so instead of silently doing nothing.
    fn do_set_options(&self, app: &AppHandle, change: impl FnOnce(&mut PlaybackOptions)) {
        if self.is_playing() {
            self.notice(app, "Stop playback to change speed or repeat.");
            self.push_status(app); // re-sync the window with the real values
            return;
        }
        change(&mut *lock(&self.options));
        self.push_status(app);
    }

    fn handle_hotkey(&self, app: &AppHandle, action: HotkeyAction) {
        match action {
            HotkeyAction::ToggleRecording => self.do_toggle_recording(app, false),
            HotkeyAction::TogglePlayback => self.do_toggle_playback(app),
            HotkeyAction::TogglePause => self.do_toggle_pause(app),
        }
    }
}

fn now_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// The only code that talks to the OS to inject input.
struct RealSimulator;

impl InputSimulator for RealSimulator {
    fn fire(&mut self, event: &EventType) {
        // Best-effort: one unmappable key must not abort the whole macro.
        let _ = rdev::simulate(event);
    }
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Runs for every raw global input event. Keep it cheap and never panic
/// (it executes inside a C callback).
fn on_input_event(state: &AppState, app: &AppHandle, event: rdev::Event) {
    let now = now_ms(event.time);

    let out = state.hotkeys.on_event(&event.event_type, now);
    if let Some(action) = out.action {
        state.handle_hotkey(app, action);
    }
    if out.swallow {
        return; // hotkey keystrokes never become part of a macro
    }

    state.recording.on_event(now, event.event_type);

    // While recording, show a live event count — but throttled, not per event.
    if state.is_recording() {
        let last = state.last_status_push_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 250 {
            state.last_status_push_ms.store(now, Ordering::Relaxed);
            state.push_status(app);
        }
    }
}

// ---------------------------------------------------------------------
// Commands (what the window's buttons call)
// ---------------------------------------------------------------------

#[tauri::command]
fn toggle_recording(app: AppHandle, state: State<AppState>) {
    state.do_toggle_recording(&app, true);
}

#[tauri::command]
fn toggle_playback(app: AppHandle, state: State<AppState>) {
    state.do_toggle_playback(&app);
}

#[tauri::command]
fn get_status(state: State<AppState>) -> Status {
    state.status()
}

/// Playback speed multiplier (sanitized to 0.1x..20x).
#[tauri::command]
fn set_speed(app: AppHandle, state: State<AppState>, speed: f64) {
    state.do_set_options(&app, |o| o.speed = clamp_speed(speed));
}

/// Number of plays; 0 means repeat until stopped.
#[tauri::command]
fn set_repeat(app: AppHandle, state: State<AppState>, repeat: u32) {
    state.do_set_options(&app, |o| o.repeat = repeat);
}

/// Save the current macro to a file the user picks (native "Save As").
/// Async because the blocking dialog must not run on the main thread.
#[tauri::command]
async fn save_macro_as(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if state.is_recording() {
        state.notice(&app, "Stop recording before saving.");
        return Ok(());
    }
    let Some(mut macro_file) = lock(&state.current_macro).clone() else {
        state.notice(&app, "Nothing to save yet — record a macro first.");
        return Ok(());
    };

    let picked = app
        .dialog()
        .file()
        .add_filter("Macroww macro", &["json"])
        .set_file_name(format!("{}.json", macro_file.name))
        .blocking_save_file();
    let Some(file_path) = picked else {
        return Ok(()); // user cancelled
    };

    let mut path = file_path.into_path().map_err(|e| e.to_string())?;
    if path.extension().is_none() {
        path.set_extension("json");
    }
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        macro_file.name = stem.to_string();
    }

    let json = serde_json::to_string_pretty(&macro_file).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("Couldn't save: {e}"))?;

    let saved_as = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    *lock(&state.current_macro) = Some(macro_file);
    state.push_status(&app);
    state.notice(&app, format!("Saved {saved_as}"));
    Ok(())
}

/// Open a saved macro from a file the user picks (native "Open").
#[tauri::command]
async fn open_macro(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if state.is_recording() || state.is_playing() {
        state.notice(&app, "Stop recording/playback before opening a macro.");
        return Ok(());
    }

    let picked = app
        .dialog()
        .file()
        .add_filter("Macroww macro", &["json"])
        .blocking_pick_file();
    let Some(file_path) = picked else {
        return Ok(()); // user cancelled
    };

    let path = file_path.into_path().map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("Couldn't read file: {e}"))?;
    let mut macro_file: MacroFile =
        serde_json::from_str(&text).map_err(|e| format!("Not a valid Macroww macro: {e}"))?;
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        macro_file.name = stem.to_string();
    }

    let name = macro_file.name.clone();
    *lock(&state.current_macro) = Some(macro_file);
    state.push_status(&app);
    state.notice(&app, format!("Opened {name}"));
    Ok(())
}

// ---------------------------------------------------------------------
// App entry point
// ---------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .setup(|app| {
            // One global input listener for the app's whole lifetime
            // (rdev has no "stop"). It feeds both the hotkey tracker and
            // the recorder, which only keeps events while recording.
            let handle = app.handle().clone();
            let state: AppState = (*app.state::<AppState>()).clone();

            std::thread::spawn(move || {
                let state_cb = state.clone();
                let handle_cb = handle.clone();
                let result = rdev::listen(move |event: rdev::Event| {
                    // A panic inside this C callback would abort the whole
                    // app, so contain it.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        on_input_event(&state_cb, &handle_cb, event)
                    }));
                    if outcome.is_err() {
                        eprintln!("macroww: input handler panicked (ignored)");
                    }
                });
                if let Err(e) = result {
                    let msg = format!(
                        "Global input capture failed to start ({e:?}). Hotkeys and recording won't work."
                    );
                    eprintln!("macroww: {msg}");
                    let _ = handle.emit("notice", msg);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            toggle_recording,
            toggle_playback,
            get_status,
            set_speed,
            set_repeat,
            save_macro_as,
            open_macro,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
