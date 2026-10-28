// Wires your four boxes to the Rust backend. The backend owns all state and
// decides what a click means (Record = start OR stop, Play = play OR stop),
// so buttons and the global hotkeys can never disagree. This file only
// draws what the backend reports.
(() => {
  const $ = (id) => document.getElementById(id);
  const btn = {
    record: $("btn-record"), play: $("btn-play"), save: $("btn-save"), open: $("btn-open"),
    repeat: $("btn-repeat"), speed: $("btn-speed"),
  };
  const img = { record: $("img-record"), play: $("img-play") };
  const statusEl = $("status");

  const tauri = window.__TAURI__;
  const invoke = tauri && tauri.core && tauri.core.invoke;
  const listen = tauri && tauri.event && tauri.event.listen;

  // Presets the Repeat / Speed boxes step through. Repeat 0 = until stopped.
  const SPEEDS = [0.5, 0.75, 1, 1.5, 2, 3, 5];
  const REPEATS = [1, 2, 3, 5, 10, 25, 100, 0];

  let status = {
    state: "idle", has_macro: false, macro_name: "", event_count: 0, duration_ms: 0,
    speed: 1, repeat: 1,
  };
  let shown = { speed: null, repeat: null }; // last values drawn, to animate changes
  let progress = null; // { done, total } while playing
  let notice = null; // transient message, shown over the status line
  let noticeTimer = null;

  function fmtDuration(ms) {
    const s = ms / 1000;
    if (s < 10) return s.toFixed(1) + "s";
    if (s < 60) return Math.round(s) + "s";
    return Math.floor(s / 60) + "m " + String(Math.round(s % 60)).padStart(2, "0") + "s";
  }

  const fmtSpeed = (v) => (Math.round(v * 100) / 100) + "×";

  function loopText() {
    if (!progress || status.repeat === 1) return "";
    return status.repeat === 0
      ? " · loop " + progress.loop_index
      : " · loop " + progress.loop_index + "/" + status.repeat;
  }

  function bump(el) {
    el.classList.remove("bump");
    void el.offsetWidth; // restart the animation
    el.classList.add("bump");
  }

  function renderOptions(busy) {
    if (shown.speed !== status.speed) {
      $("txt-speed").textContent = fmtSpeed(status.speed);
      if (shown.speed !== null) bump($("opt-speed"));
      shown.speed = status.speed;
    }
    if (shown.repeat !== status.repeat) {
      const forever = status.repeat === 0;
      $("txt-repeat").hidden = forever;
      $("img-inf").hidden = !forever;
      if (!forever) $("txt-repeat").textContent = status.repeat + "×";
      if (shown.repeat !== null) bump($("opt-repeat"));
      shown.repeat = status.repeat;
    }
    const repeatName = status.repeat === 0 ? "forever" : status.repeat + (status.repeat === 1 ? " time" : " times");
    btn.repeat.title = "Repeat: " + repeatName + " — click for more, right-click for less";
    btn.speed.title = "Speed: " + fmtSpeed(status.speed) + " — click to speed up, right-click to slow down";
    btn.repeat.classList.toggle("dim", busy);
    btn.speed.classList.toggle("dim", busy);
  }

  function statusText() {
    switch (status.state) {
      case "recording":
        return "recording… " + status.event_count + " events";
      case "playing": {
        const pct = progress && progress.total ? Math.floor((progress.done / progress.total) * 100) : 0;
        return "playing… " + pct + "%" + loopText();
      }
      case "paused":
        return "paused — Ctrl+Alt+Space to resume";
      default:
        if (!status.has_macro) return "ready";
        return (
          (status.macro_name || "untitled") +
          " · " + status.event_count + " events · " + fmtDuration(status.duration_ms)
        );
    }
  }

  function render() {
    const rec = status.state === "recording";
    const playing = status.state === "playing" || status.state === "paused";

    btn.record.classList.toggle("on", rec);
    btn.play.classList.toggle("on", playing);
    btn.play.classList.toggle("paused", status.state === "paused");
    img.record.src = rec ? "assets/stop.svg" : "assets/screen-record.svg";
    img.play.src = playing ? "assets/stop.svg" : "assets/play-arrow.svg";

    btn.record.title = rec ? "Stop recording (Ctrl+Alt+R)" : "Record (Ctrl+Alt+R)";
    btn.play.title = playing ? "Stop (Ctrl+Alt+P)" : "Play (Ctrl+Alt+P)";

    // dimmed (not disabled) so a click can still explain what's missing
    btn.play.classList.toggle("dim", !status.has_macro && !playing);
    btn.save.classList.toggle("dim", !status.has_macro || rec);
    btn.open.classList.toggle("dim", rec || playing);

    renderOptions(playing);

    statusEl.textContent = notice !== null ? notice : statusText();
  }

  function showNotice(text) {
    notice = String(text);
    render();
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => { notice = null; render(); }, 3500);
  }

  async function call(command) {
    if (!invoke) { showNotice("Not running inside the Macroww app."); return; }
    try {
      await invoke(command);
    } catch (e) {
      showNotice(e);
    }
  }

  function wire(el, command) {
    el.addEventListener("click", () => {
      call(command);
      el.blur(); // a focused button would also react to Space/Enter
    });
  }
  wire(btn.record, "toggle_recording");
  wire(btn.play, "toggle_playback");
  wire(btn.save, "save_macro_as");
  wire(btn.open, "open_macro");

  // Repeat / Speed: click steps forward through the presets, right-click
  // steps back. The backend validates and owns the values.
  function step(list, current, dir) {
    let i = list.indexOf(current);
    if (i < 0) { // not a preset: start from the nearest one
      i = 0;
      list.forEach((v, k) => { if (Math.abs(v - current) < Math.abs(list[i] - current)) i = k; });
    }
    return list[(i + dir + list.length) % list.length];
  }
  function wireOption(el, command, arg, list, currentOf) {
    const go = async (dir) => {
      if (!invoke) return;
      try { await invoke(command, { [arg]: step(list, currentOf(), dir) }); }
      catch (e) { showNotice(e); }
      el.blur();
    };
    el.addEventListener("click", () => go(+1));
    el.addEventListener("contextmenu", (e) => { e.preventDefault(); go(-1); });
  }
  wireOption(btn.speed, "set_speed", "speed", SPEEDS, () => status.speed);
  wireOption(btn.repeat, "set_repeat", "repeat", REPEATS, () => status.repeat);

  // Ctrl+Alt+Space is the pause hotkey; if this window has focus the Space
  // key must not "click" a focused button as well.
  document.addEventListener("keydown", (e) => {
    if (e.code === "Space") e.preventDefault();
  });

  // ---- updates pushed from the backend (buttons AND hotkeys land here) ----
  if (listen) {
    listen("status_changed", (e) => {
      status = e.payload;
      if (status.state !== "playing") progress = null;
      render();
    });
    listen("playback_progress", (e) => { progress = e.payload; render(); });
    listen("notice", (e) => showNotice(e.payload));
  }

  // Safety net: re-sync twice a second in case an event is ever missed.
  async function poll() {
    if (!invoke) return;
    try {
      status = await invoke("get_status");
      if (status.state !== "playing") progress = null;
      render();
    } catch (_) { /* backend not ready yet */ }
  }
  poll();
  setInterval(poll, 500);

  if (!invoke) statusEl.textContent = "open this through the Macroww app";
  else render();
})();
