//! Core recording/playback engine — platform-agnostic, no Tauri dependency.
//! Everything in here is unit-tested, and the capture/hotkey/playback paths
//! are additionally exercised against a real X server (Xvfb) during
//! development — see the README.

use rdev::{Button, EventType, Key};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Locks a mutex, recovering the data if another thread panicked while
/// holding it. The global input callback runs inside a C callback, where a
/// poisoned-lock panic would abort the whole process — so never unwrap.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------
// Macro format
// ---------------------------------------------------------------------

/// One captured input event, timestamped in milliseconds relative to the
/// start of the macro (not wall-clock), so playback speed is a division.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MacroEvent {
    pub t_ms: u64,
    /// rdev's own serializable event type — no hand-written key mapping.
    pub event: EventType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MacroFile {
    pub name: String,
    pub version: u32,
    pub platform_recorded_on: String,
    pub events: Vec<MacroEvent>,
}

impl MacroFile {
    pub fn new(name: impl Into<String>, events: Vec<MacroEvent>) -> Self {
        let platform = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "linux") {
            "linux-x11"
        } else {
            "unknown"
        };
        MacroFile {
            name: name.into(),
            version: 1,
            platform_recorded_on: platform.to_string(),
            events,
        }
    }

    pub fn duration_ms(&self) -> u64 {
        self.events.last().map(|e| e.t_ms).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------

/// Mouse moves closer together than this are coalesced (only the latest
/// position is kept). A mouse can report 500-1000 moves/second; keeping
/// all of them bloats macros and slows everything down for no benefit.
pub const MIN_MOVE_INTERVAL_MS: u64 = 8;

#[derive(Default)]
struct RecorderInner {
    events: Vec<MacroEvent>,
    /// Latest mouse position seen since the last stored move. Flushed
    /// before any non-move event (so a click always lands at the exact
    /// position the pointer was at) and on stop.
    pending_move: Option<MacroEvent>,
    last_move_t: Option<u64>,
}

/// Shared recording state. A single background thread calls `rdev::listen`
/// for the app's lifetime (rdev has no public stop), and feeds every raw OS
/// event to `on_event`, which only keeps it while `is_recording` is set.
#[derive(Clone)]
pub struct RecordingState {
    pub is_recording: Arc<AtomicBool>,
    inner: Arc<Mutex<RecorderInner>>,
    start_ms: Arc<AtomicU64>,
}

impl Default for RecordingState {
    fn default() -> Self {
        RecordingState {
            is_recording: Arc::new(AtomicBool::new(false)),
            inner: Arc::new(Mutex::new(RecorderInner::default())),
            start_ms: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl RecordingState {
    pub fn start(&self, now_ms: u64) {
        *lock(&self.inner) = RecorderInner::default();
        self.start_ms.store(now_ms, Ordering::SeqCst);
        self.is_recording.store(true, Ordering::SeqCst);
    }

    /// Stops recording and returns the raw captured events (not yet cleaned
    /// up — see `finalize_recording`).
    pub fn stop(&self) -> Vec<MacroEvent> {
        self.is_recording.store(false, Ordering::SeqCst);
        let mut g = lock(&self.inner);
        if let Some(p) = g.pending_move.take() {
            g.events.push(p);
        }
        std::mem::take(&mut g.events)
    }

    pub fn len(&self) -> usize {
        let g = lock(&self.inner);
        g.events.len() + usize::from(g.pending_move.is_some())
    }

    pub fn on_event(&self, now_ms: u64, event: EventType) {
        if !self.is_recording.load(Ordering::SeqCst) {
            return;
        }
        let t_ms = now_ms.saturating_sub(self.start_ms.load(Ordering::SeqCst));
        let mut g = lock(&self.inner);
        match event {
            EventType::MouseMove { .. } => {
                let ev = MacroEvent { t_ms, event };
                let due = g
                    .last_move_t
                    .map_or(true, |lt| t_ms.saturating_sub(lt) >= MIN_MOVE_INTERVAL_MS);
                if due {
                    g.pending_move = None;
                    g.last_move_t = Some(t_ms);
                    g.events.push(ev);
                } else {
                    g.pending_move = Some(ev);
                }
            }
            other => {
                if let Some(p) = g.pending_move.take() {
                    g.last_move_t = Some(p.t_ms);
                    g.events.push(p);
                }
                g.events.push(MacroEvent { t_ms, event: other });
            }
        }
    }
}

fn is_modifier(k: Key) -> bool {
    matches!(
        k,
        Key::ControlLeft
            | Key::ControlRight
            | Key::Alt
            | Key::AltGr
            | Key::ShiftLeft
            | Key::ShiftRight
            | Key::MetaLeft
            | Key::MetaRight
    )
}

/// When recording was stopped by clicking the app's own button, the tail of
/// the recording is the pointer travelling to that button plus the click
/// itself. Remove it, so a macro doesn't end by clicking Macroww.
fn trim_ui_stop_click(events: &mut Vec<MacroEvent>) {
    if matches!(
        events.last(),
        Some(MacroEvent { event: EventType::ButtonRelease(Button::Left), .. })
    ) {
        events.pop();
    }
    if matches!(
        events.last(),
        Some(MacroEvent { event: EventType::ButtonPress(Button::Left), .. })
    ) {
        events.pop();
        while matches!(
            events.last(),
            Some(MacroEvent { event: EventType::MouseMove { .. }, .. })
        ) {
            events.pop();
        }
    }
}

/// Drops releases that have no matching press inside the macro (e.g. the
/// release of the hotkey that *started* the recording).
fn drop_orphan_releases(events: Vec<MacroEvent>) -> Vec<MacroEvent> {
    let mut keys: Vec<Key> = vec![];
    let mut buttons: Vec<Button> = vec![];
    events
        .into_iter()
        .filter(|ev| match &ev.event {
            EventType::KeyPress(k) => {
                if !keys.contains(k) {
                    keys.push(*k);
                }
                true
            }
            EventType::KeyRelease(k) => match keys.iter().position(|x| x == k) {
                Some(i) => {
                    keys.remove(i);
                    true
                }
                None => false,
            },
            EventType::ButtonPress(b) => {
                if !buttons.contains(b) {
                    buttons.push(*b);
                }
                true
            }
            EventType::ButtonRelease(b) => match buttons.iter().position(|x| x == b) {
                Some(i) => {
                    buttons.remove(i);
                    true
                }
                None => false,
            },
            _ => true,
        })
        .collect()
}

/// Removes modifier presses that are never released inside the macro. When
/// a recording is stopped with a hotkey like Ctrl+Alt+R, the Ctrl and Alt
/// presses are captured but their releases happen after recording stops —
/// leaving them in would make playback end with Ctrl+Alt held down.
fn drop_unreleased_modifiers(events: Vec<MacroEvent>) -> Vec<MacroEvent> {
    let mut open: Vec<(Key, usize)> = vec![];
    for (i, ev) in events.iter().enumerate() {
        match &ev.event {
            EventType::KeyPress(k) if is_modifier(*k) => {
                if !open.iter().any(|(ok, _)| ok == k) {
                    open.push((*k, i));
                }
            }
            EventType::KeyRelease(k) => {
                if let Some(p) = open.iter().position(|(ok, _)| ok == k) {
                    open.remove(p);
                }
            }
            _ => {}
        }
    }
    let drop_idx: Vec<usize> = open.into_iter().map(|(_, i)| i).collect();
    events
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !drop_idx.contains(i))
        .map(|(_, e)| e)
        .collect()
}

/// Cleans up a raw recording: removes the artifacts of how recording was
/// started/stopped (the stop click or hotkey chord), and shifts time so the
/// macro begins at t=0 instead of replaying the idle time before the first
/// action.
pub fn finalize_recording(mut events: Vec<MacroEvent>, stopped_via_ui: bool) -> Vec<MacroEvent> {
    if stopped_via_ui {
        trim_ui_stop_click(&mut events);
    }
    let mut events = drop_unreleased_modifiers(drop_orphan_releases(events));
    if let Some(first) = events.first().map(|e| e.t_ms) {
        for e in events.iter_mut() {
            e.t_ms -= first;
        }
    }
    events
}

// ---------------------------------------------------------------------
// Global hotkeys
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    ToggleRecording, // Ctrl+Alt+R
    TogglePlayback,  // Ctrl+Alt+P
    TogglePause,     // Ctrl+Alt+Space
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyOutcome {
    /// Run this action now.
    pub action: Option<HotkeyAction>,
    /// Keep this event out of any recording in progress.
    pub swallow: bool,
}

/// A press arriving this soon after a release of the same key is X11's
/// auto-repeat (it reports repeats as a release+press pair with the same
/// timestamp), not a new keystroke. A real re-tap takes far longer.
pub const REPEAT_PAIR_MS: u64 = 20;

/// If a key has been "down" with no events at all for this long, assume we
/// missed its release and stop treating it as held.
pub const STALE_HOLD_MS: u64 = 2000;

#[derive(Default, Clone, Copy)]
struct KeySlot {
    down: bool,
    last_event_ms: u64,
    last_release_ms: u64,
}

/// Tracks modifier state from the raw event stream (rdev reports plain
/// press/release, not a modifier mask) and recognizes Ctrl+Alt+<key>.
///
/// Auto-repeat is the subtle part: hold a key and X11 sends *nothing* for
/// its repeat delay (~660ms), then a stream of repeats. So "quiet for a
/// while" can't tell a fresh press from a repeat — instead each trigger
/// key's down/up state is tracked, and a press while it is already down
/// (or right on the heels of its release) is a repeat. Holding Ctrl+Alt+R
/// therefore fires exactly once.
#[derive(Clone)]
pub struct HotkeyTracker {
    ctrl_down: Arc<AtomicBool>,
    alt_down: Arc<AtomicBool>,
    slots: Arc<Mutex<[KeySlot; 3]>>,
}

impl Default for HotkeyTracker {
    fn default() -> Self {
        HotkeyTracker {
            ctrl_down: Arc::new(AtomicBool::new(false)),
            alt_down: Arc::new(AtomicBool::new(false)),
            slots: Arc::new(Mutex::new([KeySlot::default(); 3])),
        }
    }
}

impl HotkeyTracker {
    pub fn on_event(&self, ev: &EventType, now_ms: u64) -> HotkeyOutcome {
        const NONE: HotkeyOutcome = HotkeyOutcome { action: None, swallow: false };
        match ev {
            EventType::KeyPress(Key::ControlLeft) | EventType::KeyPress(Key::ControlRight) => {
                self.ctrl_down.store(true, Ordering::SeqCst)
            }
            EventType::KeyRelease(Key::ControlLeft) | EventType::KeyRelease(Key::ControlRight) => {
                self.ctrl_down.store(false, Ordering::SeqCst)
            }
            EventType::KeyPress(Key::Alt) | EventType::KeyPress(Key::AltGr) => {
                self.alt_down.store(true, Ordering::SeqCst)
            }
            EventType::KeyRelease(Key::Alt) | EventType::KeyRelease(Key::AltGr) => {
                self.alt_down.store(false, Ordering::SeqCst)
            }
            _ => {}
        }

        let (is_press, key) = match ev {
            EventType::KeyPress(k) => (true, *k),
            EventType::KeyRelease(k) => (false, *k),
            _ => return NONE,
        };
        let (slot_idx, action) = match key {
            Key::KeyR => (0, HotkeyAction::ToggleRecording),
            Key::KeyP => (1, HotkeyAction::TogglePlayback),
            Key::Space => (2, HotkeyAction::TogglePause),
            _ => return NONE,
        };

        let mut slots = lock(&self.slots);
        let slot = &mut slots[slot_idx];

        if !is_press {
            slot.down = false;
            slot.last_release_ms = now_ms;
            slot.last_event_ms = now_ms;
            return NONE;
        }

        let stale = slot.down && now_ms.saturating_sub(slot.last_event_ms) > STALE_HOLD_MS;
        let held_already = slot.down && !stale;
        let right_after_release = slot.last_release_ms != 0
            && now_ms.saturating_sub(slot.last_release_ms) <= REPEAT_PAIR_MS;
        let is_repeat = held_already || right_after_release;
        slot.down = true;
        slot.last_event_ms = now_ms;
        drop(slots);

        let chord = self.ctrl_down.load(Ordering::SeqCst) && self.alt_down.load(Ordering::SeqCst);
        if chord {
            HotkeyOutcome { action: if is_repeat { None } else { Some(action) }, swallow: true }
        } else {
            NONE
        }
    }
}

// ---------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------

#[derive(Clone)]
pub struct PlaybackControl {
    pub is_playing: Arc<AtomicBool>,
    pub is_paused: Arc<AtomicBool>,
    pub stop_requested: Arc<AtomicBool>,
}

impl Default for PlaybackControl {
    fn default() -> Self {
        PlaybackControl {
            is_playing: Arc::new(AtomicBool::new(false)),
            is_paused: Arc::new(AtomicBool::new(false)),
            stop_requested: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl PlaybackControl {
    /// Resets flags for a fresh run. Callers MUST call this synchronously,
    /// before spawning the playback thread, so a stop request issued right
    /// after start can't be wiped out by the new thread's startup.
    pub fn begin(&self) {
        self.is_paused.store(false, Ordering::SeqCst);
        self.stop_requested.store(false, Ordering::SeqCst);
        self.is_playing.store(true, Ordering::SeqCst);
    }
}

/// `repeat` value meaning "keep looping until stopped".
pub const REPEAT_FOREVER: u32 = 0;

/// Pause between repeats. Keeps a macro whose events all happen at once
/// from spinning at full CPU speed forever, and gives every loop a clean
/// start. Interruptible like any other wait.
pub const REPEAT_GAP: Duration = Duration::from_millis(50);

pub const MIN_SPEED: f64 = 0.1;
pub const MAX_SPEED: f64 = 20.0;

/// Sanitizes a playback speed multiplier: NaN, infinite, zero or negative
/// values mean 1x; everything else is limited to 0.1x..20x.
pub fn clamp_speed(speed: f64) -> f64 {
    if !speed.is_finite() || speed <= 0.0 {
        1.0
    } else {
        speed.clamp(MIN_SPEED, MAX_SPEED)
    }
}

/// Turns recorded events into "wait this long, then fire this event" steps
/// with the speed multiplier applied. Pure, so it's unit-testable.
pub fn build_playback_schedule(events: &[MacroEvent], speed: f64) -> Vec<(Duration, EventType)> {
    let speed = clamp_speed(speed);
    let mut schedule = Vec::with_capacity(events.len());
    let mut prev_t = 0u64;
    for ev in events {
        let delta_ms = ev.t_ms.saturating_sub(prev_t);
        let scaled_ms = (delta_ms as f64 / speed).round() as u64;
        schedule.push((Duration::from_millis(scaled_ms), ev.event.clone()));
        prev_t = ev.t_ms;
    }
    schedule
}

/// "Actually fire an input event" — abstracted so playback logic can be
/// tested with a mock instead of the real OS.
pub trait InputSimulator {
    fn fire(&mut self, event: &EventType);
    fn sleep(&mut self, d: Duration);
}

/// Keys/buttons the macro has pressed but not yet released, so playback can
/// release them if it ends or is stopped mid-way (otherwise a stopped macro
/// can leave a key or mouse button stuck down system-wide).
#[derive(Default)]
struct HeldInput {
    keys: Vec<Key>,
    buttons: Vec<Button>,
}

impl HeldInput {
    fn observe(&mut self, ev: &EventType) {
        match ev {
            EventType::KeyPress(k) => {
                if !self.keys.contains(k) {
                    self.keys.push(*k);
                }
            }
            EventType::KeyRelease(k) => self.keys.retain(|x| x != k),
            EventType::ButtonPress(b) => {
                if !self.buttons.contains(b) {
                    self.buttons.push(*b);
                }
            }
            EventType::ButtonRelease(b) => self.buttons.retain(|x| x != b),
            _ => {}
        }
    }

    fn release_all(&mut self, sim: &mut impl InputSimulator) {
        for k in self.keys.drain(..) {
            sim.fire(&EventType::KeyRelease(k));
        }
        for b in self.buttons.drain(..) {
            sim.fire(&EventType::ButtonRelease(b));
        }
    }
}

/// How often playback re-checks stop/pause while waiting out a delay, so
/// Stop/Pause take effect within ~30ms even across multi-second gaps.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(30);

/// Waits `d`, in small chunks, honoring pause and stop the whole time.
/// Returns `false` if playback was stopped. Pause/stop are checked even when
/// `d` is zero, so a pause takes effect immediately.
fn wait_interruptible(
    d: Duration,
    control: &PlaybackControl,
    sim: &mut impl InputSimulator,
) -> bool {
    let mut remaining = d;
    loop {
        if control.stop_requested.load(Ordering::SeqCst) {
            return false;
        }
        while control.is_paused.load(Ordering::SeqCst) {
            if control.stop_requested.load(Ordering::SeqCst) {
                return false;
            }
            sim.sleep(STOP_POLL_INTERVAL);
        }
        if remaining.is_zero() {
            return true;
        }
        let chunk = remaining.min(STOP_POLL_INTERVAL);
        sim.sleep(chunk);
        remaining -= chunk;
    }
}

fn run_playback_pass(
    schedule: &[(Duration, EventType)],
    control: &PlaybackControl,
    sim: &mut impl InputSimulator,
    held: &mut HeldInput,
    mut on_progress: impl FnMut(usize, usize),
) -> bool {
    let total = schedule.len();
    for (i, (delay, event)) in schedule.iter().enumerate() {
        if !wait_interruptible(*delay, control, sim) {
            return false;
        }
        sim.fire(event);
        held.observe(event);
        on_progress(i + 1, total);
    }
    true
}

/// Plays the macro `loop_count` times, or until stopped if `loop_count` is
/// `REPEAT_FOREVER`. `on_progress(done, total, loop_index, loop_count)`.
/// Whatever happens, anything still held down is released at the end.
pub fn run_playback(
    events: &[MacroEvent],
    speed: f64,
    loop_count: u32,
    control: &PlaybackControl,
    sim: &mut impl InputSimulator,
    mut on_progress: impl FnMut(usize, usize, u32, u32),
) {
    // Note: does NOT reset control flags — see `PlaybackControl::begin`.
    let schedule = build_playback_schedule(events, speed);
    let forever = loop_count == REPEAT_FOREVER;
    let mut held = HeldInput::default();
    let mut loop_index: u32 = 0;

    loop {
        if control.stop_requested.load(Ordering::SeqCst) {
            break;
        }
        loop_index = loop_index.saturating_add(1);
        let completed = run_playback_pass(&schedule, control, sim, &mut held, |done, total| {
            on_progress(done, total, loop_index, loop_count)
        });
        if !completed {
            break;
        }
        if !forever && loop_index >= loop_count {
            break;
        }
        if !wait_interruptible(REPEAT_GAP, control, sim) {
            break;
        }
    }

    held.release_all(sim);
    control.is_playing.store(false, Ordering::SeqCst);
    control.is_paused.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t_ms: u64, event: EventType) -> MacroEvent {
        MacroEvent { t_ms, event }
    }
    fn mv(t: u64, x: f64) -> MacroEvent {
        ev(t, EventType::MouseMove { x, y: 0.0 })
    }
    fn sample_events() -> Vec<MacroEvent> {
        vec![
            mv(0, 100.0),
            ev(120, EventType::ButtonPress(Button::Left)),
            ev(180, EventType::ButtonRelease(Button::Left)),
            ev(400, EventType::KeyPress(Key::KeyA)),
            ev(460, EventType::KeyRelease(Key::KeyA)),
        ]
    }

    // ---- format ----

    #[test]
    fn macro_file_json_roundtrip() {
        let mf = MacroFile::new("Test Macro", sample_events());
        let json = serde_json::to_string_pretty(&mf).unwrap();
        let parsed: MacroFile = serde_json::from_str(&json).unwrap();
        assert_eq!(mf, parsed);
        assert_eq!(mf.duration_ms(), 460);
    }

    // ---- recording ----

    #[test]
    fn recording_ignores_events_when_stopped() {
        let state = RecordingState::default();
        state.on_event(1000, EventType::KeyPress(Key::KeyA));
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn recording_captures_relative_timestamps() {
        let state = RecordingState::default();
        state.start(1000);
        state.on_event(1000, EventType::MouseMove { x: 1.0, y: 1.0 });
        state.on_event(1120, EventType::ButtonPress(Button::Left));
        state.on_event(1400, EventType::KeyPress(Key::KeyA));
        let events = state.stop();
        assert_eq!(events.iter().map(|e| e.t_ms).collect::<Vec<_>>(), vec![0, 120, 400]);
        state.on_event(2000, EventType::KeyRelease(Key::KeyA));
        assert_eq!(state.len(), 0, "events after stop must not be captured");
    }

    #[test]
    fn mouse_moves_are_coalesced_but_final_position_is_kept() {
        let state = RecordingState::default();
        state.start(0);
        for t in [0u64, 2, 4, 6, 8, 10] {
            state.on_event(t, EventType::MouseMove { x: t as f64, y: 0.0 });
        }
        let events = state.stop();
        // t=0 stored, 2/4/6 coalesced, t=8 stored, t=10 pending -> flushed on stop
        assert_eq!(events.iter().map(|e| e.t_ms).collect::<Vec<_>>(), vec![0, 8, 10]);
    }

    #[test]
    fn pending_move_is_flushed_before_a_click() {
        let state = RecordingState::default();
        state.start(0);
        state.on_event(0, EventType::MouseMove { x: 1.0, y: 0.0 });
        state.on_event(3, EventType::MouseMove { x: 50.0, y: 0.0 }); // coalesced
        state.on_event(4, EventType::ButtonPress(Button::Left));
        let events = state.stop();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].event, EventType::MouseMove { x: 50.0, y: 0.0 });
        assert_eq!(events[2].event, EventType::ButtonPress(Button::Left));
    }

    #[test]
    fn a_mouse_flood_stays_small() {
        // 1000 moves/sec for 5 seconds must not produce 5000 stored events.
        let state = RecordingState::default();
        state.start(0);
        for t in 0..5000u64 {
            state.on_event(t, EventType::MouseMove { x: t as f64, y: 0.0 });
        }
        assert!(state.stop().len() <= 5000 / MIN_MOVE_INTERVAL_MS as usize + 2);
    }

    // ---- finalize ----

    #[test]
    fn finalize_strips_hotkey_chord_artifacts() {
        // Started with Ctrl+Alt+R (releases land inside the recording),
        // stopped with Ctrl+Alt+R (presses land inside, releases don't).
        let raw = vec![
            ev(10, EventType::KeyRelease(Key::KeyR)),
            ev(20, EventType::KeyRelease(Key::Alt)),
            ev(30, EventType::KeyRelease(Key::ControlLeft)),
            mv(500, 5.0),
            ev(600, EventType::ButtonPress(Button::Left)),
            ev(650, EventType::ButtonRelease(Button::Left)),
            ev(900, EventType::KeyPress(Key::ControlLeft)),
            ev(910, EventType::KeyPress(Key::Alt)),
        ];
        let out = finalize_recording(raw, false);
        assert_eq!(out.len(), 3, "got {:?}", out);
        assert_eq!(out[0].t_ms, 0, "macro should start at t=0");
        assert_eq!(out[2].t_ms, 150);
    }

    #[test]
    fn finalize_keeps_modifiers_that_are_properly_released() {
        let raw = vec![
            ev(0, EventType::KeyPress(Key::ControlLeft)),
            ev(10, EventType::KeyPress(Key::KeyC)),
            ev(20, EventType::KeyRelease(Key::KeyC)),
            ev(30, EventType::KeyRelease(Key::ControlLeft)),
        ];
        assert_eq!(finalize_recording(raw.clone(), false), raw);
    }

    #[test]
    fn finalize_trims_the_click_on_the_apps_own_stop_button() {
        let raw = vec![
            ev(0, EventType::ButtonPress(Button::Left)),
            ev(40, EventType::ButtonRelease(Button::Left)),
            mv(100, 1.0),
            mv(120, 2.0),
            ev(300, EventType::ButtonPress(Button::Left)), // click on Stop
        ];
        let out = finalize_recording(raw, true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].event, EventType::ButtonRelease(Button::Left));
    }

    #[test]
    fn finalize_trims_stop_click_when_release_was_also_captured() {
        let raw = vec![
            ev(0, EventType::KeyPress(Key::KeyA)),
            ev(10, EventType::KeyRelease(Key::KeyA)),
            mv(100, 1.0),
            ev(300, EventType::ButtonPress(Button::Left)),
            ev(340, EventType::ButtonRelease(Button::Left)),
        ];
        assert_eq!(finalize_recording(raw, true).len(), 2);
    }

    #[test]
    fn finalize_does_not_trim_clicks_when_stopped_by_hotkey() {
        let raw = vec![
            ev(0, EventType::ButtonPress(Button::Left)),
            ev(40, EventType::ButtonRelease(Button::Left)),
        ];
        assert_eq!(finalize_recording(raw, false).len(), 2);
    }

    // ---- hotkeys ----

    fn press(t: &HotkeyTracker, k: Key, now: u64) -> HotkeyOutcome {
        t.on_event(&EventType::KeyPress(k), now)
    }
    fn release(t: &HotkeyTracker, k: Key, now: u64) -> HotkeyOutcome {
        t.on_event(&EventType::KeyRelease(k), now)
    }

    #[test]
    fn hotkey_fires_once_and_is_swallowed() {
        let t = HotkeyTracker::default();
        assert_eq!(press(&t, Key::ControlLeft, 1000).action, None);
        assert_eq!(press(&t, Key::Alt, 1010).action, None);
        let out = press(&t, Key::KeyR, 1020);
        assert_eq!(out.action, Some(HotkeyAction::ToggleRecording));
        assert!(out.swallow);
    }

    #[test]
    fn hotkey_requires_both_modifiers() {
        let t = HotkeyTracker::default();
        press(&t, Key::ControlLeft, 1000);
        let out = press(&t, Key::KeyR, 1020);
        assert_eq!(out.action, None);
        assert!(!out.swallow, "plain Ctrl+R must still reach recordings");
    }

    #[test]
    fn held_hotkey_fires_once_even_across_the_initial_repeat_delay() {
        // What X11 really does (captured from a live server): the first
        // press, then NOTHING for ~660ms, then a stream of repeats. A
        // "quiet period" debounce wrongly fires again on the first repeat.
        let t = HotkeyTracker::default();
        press(&t, Key::ControlLeft, 0);
        press(&t, Key::Alt, 5);
        let mut fired = 0;
        if press(&t, Key::KeyR, 100).action.is_some() {
            fired += 1;
        }
        let mut now = 100 + 666;
        for _ in 0..40 {
            if press(&t, Key::KeyR, now).action.is_some() {
                fired += 1;
            }
            now += 33;
        }
        release(&t, Key::KeyR, now);
        assert_eq!(fired, 1);
    }

    #[test]
    fn auto_repeat_reported_as_release_press_pairs_fires_once() {
        // Xorg on real keyboards reports repeats as release+press pairs
        // with the same timestamp.
        let t = HotkeyTracker::default();
        press(&t, Key::ControlLeft, 0);
        press(&t, Key::Alt, 5);
        let mut fired = 0;
        if press(&t, Key::KeyR, 100).action.is_some() {
            fired += 1;
        }
        let mut now = 100 + 660;
        for _ in 0..40 {
            release(&t, Key::KeyR, now);
            if press(&t, Key::KeyR, now).action.is_some() {
                fired += 1;
            }
            now += 33;
        }
        assert_eq!(fired, 1);
    }

    #[test]
    fn repeated_presses_are_still_swallowed_from_the_recording() {
        let t = HotkeyTracker::default();
        press(&t, Key::ControlLeft, 0);
        press(&t, Key::Alt, 5);
        press(&t, Key::KeyR, 100);
        let repeat = press(&t, Key::KeyR, 800);
        assert_eq!(repeat.action, None);
        assert!(repeat.swallow);
    }

    #[test]
    fn hotkey_can_fire_again_after_the_key_is_let_go() {
        let t = HotkeyTracker::default();
        press(&t, Key::ControlLeft, 0);
        press(&t, Key::Alt, 5);
        assert!(press(&t, Key::KeyR, 100).action.is_some());
        release(&t, Key::KeyR, 180);
        assert!(press(&t, Key::KeyR, 1500).action.is_some());
    }

    #[test]
    fn a_missed_release_does_not_disable_the_hotkey_forever() {
        let t = HotkeyTracker::default();
        press(&t, Key::ControlLeft, 0);
        press(&t, Key::Alt, 5);
        assert!(press(&t, Key::KeyR, 100).action.is_some());
        // release never observed; much later the user presses it again
        assert!(press(&t, Key::KeyR, 100 + STALE_HOLD_MS + 500).action.is_some());
    }

    #[test]
    fn modifier_state_clears_on_release() {
        let t = HotkeyTracker::default();
        press(&t, Key::ControlLeft, 0);
        press(&t, Key::Alt, 5);
        release(&t, Key::Alt, 50);
        assert_eq!(press(&t, Key::KeyP, 2000).action, None);
    }

    // ---- playback ----

    #[test]
    fn schedule_applies_speed_multiplier() {
        let schedule = build_playback_schedule(&sample_events(), 2.0);
        assert_eq!(schedule[1].0, Duration::from_millis(60));
        assert_eq!(schedule[3].0, Duration::from_millis(110));
    }

    #[test]
    fn schedule_ignores_zero_or_negative_speed() {
        let e = sample_events();
        assert_eq!(build_playback_schedule(&e, 1.0), build_playback_schedule(&e, 0.0));
    }

    struct MockSim {
        fired: Vec<EventType>,
        elapsed: Duration,
    }
    impl MockSim {
        fn new() -> Self {
            MockSim { fired: vec![], elapsed: Duration::ZERO }
        }
    }
    impl InputSimulator for MockSim {
        fn fire(&mut self, event: &EventType) {
            self.fired.push(event.clone());
        }
        fn sleep(&mut self, d: Duration) {
            self.elapsed += d;
        }
    }

    #[test]
    fn playback_fires_every_event_in_order() {
        let control = PlaybackControl::default();
        control.begin();
        let mut sim = MockSim::new();
        let mut last = None;
        run_playback(&sample_events(), 1.0, 1, &control, &mut sim, |d, t, l, lt| {
            last = Some((d, t, l, lt))
        });
        assert_eq!(sim.fired.len(), 5);
        assert_eq!(last, Some((5, 5, 1, 1)));
        assert!(!control.is_playing.load(Ordering::SeqCst));
    }

    #[test]
    fn playback_honors_loop_count() {
        let control = PlaybackControl::default();
        control.begin();
        let mut sim = MockSim::new();
        run_playback(&sample_events(), 1.0, 3, &control, &mut sim, |_, _, _, _| {});
        assert_eq!(sim.fired.len(), 15);
    }

    #[test]
    fn clamp_speed_sanitizes_bad_values() {
        assert_eq!(clamp_speed(f64::NAN), 1.0);
        assert_eq!(clamp_speed(f64::INFINITY), 1.0);
        assert_eq!(clamp_speed(-3.0), 1.0);
        assert_eq!(clamp_speed(0.0), 1.0);
        assert_eq!(clamp_speed(0.01), MIN_SPEED);
        assert_eq!(clamp_speed(500.0), MAX_SPEED);
        assert_eq!(clamp_speed(2.5), 2.5);
    }

    #[test]
    fn nan_speed_does_not_collapse_all_delays_to_zero() {
        let schedule = build_playback_schedule(&sample_events(), f64::NAN);
        assert_eq!(schedule[1].0, Duration::from_millis(120));
    }

    struct StopAfterFires {
        inner: MockSim,
        limit: usize,
        flag: Arc<AtomicBool>,
    }
    impl InputSimulator for StopAfterFires {
        fn fire(&mut self, e: &EventType) {
            self.inner.fire(e);
            if self.inner.fired.len() >= self.limit {
                self.flag.store(true, Ordering::SeqCst);
            }
        }
        fn sleep(&mut self, d: Duration) {
            self.inner.sleep(d)
        }
    }

    #[test]
    fn repeat_forever_keeps_looping_until_stopped() {
        // 5 events per pass; stop is requested right as pass 2 finishes, so
        // this also proves a stop landing in the gap between repeats works.
        let control = PlaybackControl::default();
        control.begin();
        let mut sim = StopAfterFires {
            inner: MockSim::new(),
            limit: 10,
            flag: control.stop_requested.clone(),
        };
        run_playback(&sample_events(), 1.0, REPEAT_FOREVER, &control, &mut sim, |_, _, _, _| {});
        assert_eq!(sim.inner.fired.len(), 10);
        assert!(!control.is_playing.load(Ordering::SeqCst));
    }

    #[test]
    fn stopping_a_forever_loop_mid_pass_releases_held_input() {
        // limit 12 lands right after the left button goes down in pass 3.
        let control = PlaybackControl::default();
        control.begin();
        let mut sim = StopAfterFires {
            inner: MockSim::new(),
            limit: 12,
            flag: control.stop_requested.clone(),
        };
        run_playback(&sample_events(), 1.0, REPEAT_FOREVER, &control, &mut sim, |_, _, _, _| {});
        assert_eq!(sim.inner.fired.last(), Some(&EventType::ButtonRelease(Button::Left)));
    }

    #[test]
    fn repeats_are_separated_by_a_gap_even_for_instant_macros() {
        // All events at t=0: without a gap, "repeat forever" would spin at
        // full CPU speed. 3 loops => 2 gaps.
        let events = vec![
            ev(0, EventType::KeyPress(Key::KeyA)),
            ev(0, EventType::KeyRelease(Key::KeyA)),
        ];
        let control = PlaybackControl::default();
        control.begin();
        let mut sim = MockSim::new();
        run_playback(&events, 1.0, 3, &control, &mut sim, |_, _, _, _| {});
        assert_eq!(sim.fired.len(), 6);
        assert!(sim.elapsed >= REPEAT_GAP * 2, "elapsed {:?}", sim.elapsed);
    }

    #[test]
    fn progress_reports_loop_index_and_total() {
        let control = PlaybackControl::default();
        control.begin();
        let mut sim = MockSim::new();
        let mut seen = vec![];
        run_playback(&sample_events(), 1.0, 2, &control, &mut sim, |_, _, l, lt| seen.push((l, lt)));
        assert_eq!(seen.first(), Some(&(1, 2)));
        assert_eq!(seen.last(), Some(&(2, 2)));
    }

    #[test]
    fn playback_stops_early_when_requested() {
        let control = PlaybackControl::default();
        control.begin();
        control.stop_requested.store(true, Ordering::SeqCst);
        let mut sim = MockSim::new();
        run_playback(&sample_events(), 1.0, 5, &control, &mut sim, |_, _, _, _| {});
        assert_eq!(sim.fired.len(), 0);
        assert!(!control.is_playing.load(Ordering::SeqCst));
    }

    struct StopAfterSim {
        inner: MockSim,
        stop_at: Duration,
        flag: Arc<AtomicBool>,
    }
    impl InputSimulator for StopAfterSim {
        fn fire(&mut self, e: &EventType) {
            self.inner.fire(e)
        }
        fn sleep(&mut self, d: Duration) {
            self.inner.sleep(d);
            if self.inner.elapsed >= self.stop_at {
                self.flag.store(true, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn stop_interrupts_mid_delay_not_just_between_events() {
        let events = vec![
            ev(0, EventType::KeyPress(Key::KeyA)),
            ev(10_000, EventType::KeyPress(Key::KeyB)), // 10s gap
        ];
        let control = PlaybackControl::default();
        control.begin();
        let mut sim = StopAfterSim {
            inner: MockSim::new(),
            stop_at: Duration::from_millis(200),
            flag: control.stop_requested.clone(),
        };
        run_playback(&events, 1.0, 1, &control, &mut sim, |_, _, _, _| {});
        assert!(sim.inner.elapsed < Duration::from_millis(500), "kept sleeping {:?}", sim.inner.elapsed);
        assert!(!sim.inner.fired.contains(&EventType::KeyPress(Key::KeyB)));
    }

    #[test]
    fn stopping_mid_macro_releases_held_keys_and_buttons() {
        // Stop lands while Ctrl and the left button are down: they must be
        // released, or the user is left with a stuck modifier / drag.
        let events = vec![
            ev(0, EventType::KeyPress(Key::ControlLeft)),
            ev(0, EventType::ButtonPress(Button::Left)),
            ev(10_000, EventType::KeyRelease(Key::ControlLeft)),
        ];
        let control = PlaybackControl::default();
        control.begin();
        let mut sim = StopAfterSim {
            inner: MockSim::new(),
            stop_at: Duration::from_millis(100),
            flag: control.stop_requested.clone(),
        };
        run_playback(&events, 1.0, 1, &control, &mut sim, |_, _, _, _| {});
        assert!(sim.inner.fired.contains(&EventType::KeyRelease(Key::ControlLeft)));
        assert!(sim.inner.fired.contains(&EventType::ButtonRelease(Button::Left)));
    }

    #[test]
    fn pause_blocks_progress_until_resumed() {
        struct UnpauseSim {
            inner: MockSim,
            sleeps: u32,
            flag: Arc<AtomicBool>,
        }
        impl InputSimulator for UnpauseSim {
            fn fire(&mut self, e: &EventType) {
                self.inner.fire(e)
            }
            fn sleep(&mut self, d: Duration) {
                self.sleeps += 1;
                if self.sleeps >= 3 {
                    self.flag.store(false, Ordering::SeqCst);
                }
                self.inner.sleep(d);
            }
        }
        let control = PlaybackControl::default();
        control.begin();
        control.is_paused.store(true, Ordering::SeqCst);
        let mut sim = UnpauseSim { inner: MockSim::new(), sleeps: 0, flag: control.is_paused.clone() };
        run_playback(&sample_events(), 1.0, 1, &control, &mut sim, |_, _, _, _| {});
        assert_eq!(sim.inner.fired.len(), 5);
        assert!(sim.sleeps >= 3);
    }
}
