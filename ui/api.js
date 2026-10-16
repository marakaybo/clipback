// Связь интерфейса с ядром.
// В приложении (Tauri) — настоящие команды Rust; в обычном браузере — демо-данные,
// чтобы интерфейс можно было смотреть и проверять без сборки.

(function () {
  const T = window.__TAURI__;

  if (T) {
    window.api = {
      real: true,
      call: (cmd, args) => T.core.invoke(cmd, args),
      on: (event, fn) => T.event.listen(event, (e) => fn(e.payload)),
      fileUrl: (path) => T.core.convertFileSrc(path),
    };
    // ошибки интерфейса — в журнал программы
    const report = (msg) => T.core.invoke("frontend_log", { message: String(msg) }).catch(() => {});
    window.addEventListener("error", (e) => report(`${e.message} @ ${e.filename}:${e.lineno}`));
    window.addEventListener("unhandledrejection", (e) => report(`promise: ${e.reason && (e.reason.stack || e.reason)}`));
    return;
  }

  // ---------------- демо ----------------
  const listeners = {};
  const emit = (event, payload) => (listeners[event] || []).forEach((fn) => fn(payload));
  const rnd = () => Math.floor(Math.random() * 1e6);
  const now = Date.now();
  const H = 3600e3;

  let seq = 1;
  const mk = (name, ageMs, duration, hue, kind = "video", extra = {}) => ({
    path: `demo://${seq++}`,
    name,
    kind,
    size: kind === "video" ? Math.round(duration * 2.6e6) : 3.4e6,
    modified: now - ageMs,
    duration,
    width: 2560,
    height: 1440,
    fps: kind === "video" ? 60 : 0,
    broken: false,
    audioTracks: kind === "video" ? 4 : 0,
    audioNames: kind === "video" ? ["Всё вместе", "Звук системы", "Микрофон", "Discord"] : [],
    demo: true,
    hue,
    seed: rnd(),
    markers: [],
    ...extra,
  });

  const stamp = (ms) => {
    const d = new Date(ms);
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}-${p(d.getMinutes())}-${p(d.getSeconds())}`;
  };

  const clips = [
    mk(`Battlefield 6 ${stamp(now - 0.2 * H)}`, 0.2 * H, 300, 28, "video", { markers: [254] }),
    mk("Танк с двух выстрелов", 0.5 * H, 32, 28),
    mk(`Battlefield 6 ${stamp(now - 0.6 * H)}`, 0.6 * H, 300, 200),
    mk(`Minecraft 1.21.1 ${stamp(now - 2.5 * H)}`, 2.5 * H, 180, 110),
    mk(`Minecraft 1.21.1 ${stamp(now - 26 * H)}`, 26 * H, 300, 140),
    mk(`Battlefield 6 ${stamp(now - 27 * H)}`, 27 * H, 0, 35, "shot"),
  ];

  const settings = {
    bufferMinutes: 5, fps: 60, quality: "medium", maxHeight: null, monitor: 0,
    outDir: "C:\\Users\\you\\Videos\\Clipback",
    hotkeyOverlay: "Alt+Z", hotkeySave: "Alt+F10", hotkeyScreenshot: "Alt+F1", hotkeyRecord: "Alt+F9",
    systemAudio: true, mic: true, micDevice: null, micGain: 1, audioMode: "advanced", customBitrate: 12,
    appTracks: [{ name: "Discord", exe: "Discord.exe", enabled: true }],
    recordOnStart: true, closeToTray: true, autostart: false, checkUpdates: true, screenshotFormat: "jpg", saveSound: true, recordIndicator: true, recordCursor: true, micAgc: true, confirmDelete: true, screenshotClipboard: true, autoClean: false, cleanMaxGb: 50, cleanMaxDays: 0,
  };
  const state = {
    recording: true, starting: false,
    capture: { encoder: "NVIDIA H.264 Encoder MFT", gpu: "NVIDIA GeForce RTX 5060 Ti", monitor: "DISPLAY1", width: 2560, height: 1440, fps: 60, audio: ["Всё вместе", "Звук системы", "Микрофон"], audioError: null },
    status: { bufferedSecs: 0, bufferBytes: 0, fps: 60, dropped: 0 },
    error: null, lastGame: "Battlefield 6", hotkeyErrors: [], settings, version: "0.1.0", portable: true,
  };
  const started = Date.now();
  setInterval(() => {
    if (!state.recording) return;
    const s = Math.min(settings.bufferMinutes * 60, (Date.now() - started) / 1000 + 200);
    state.status = { bufferedSecs: s, bufferBytes: s * 2.6e6, fps: 60, dropped: 0 };
    emit("state", state);
  }, 2000);

  const find = (path) => clips.find((c) => c.path === path);
  const trashBin = {};
  let trashSeq = 0;

  const handlers = {
    get_state: () => state,
    save_edited_image: ({ path, replace }) => {
      const c = find(path);
      if (replace) { c.modified = Date.now(); return { ...c }; }
      const copy = { ...c, path: `demo://${seq++}`, name: `${c.name} — правка`, modified: Date.now() };
      clips.push(copy);
      emit("clips-changed");
      return copy;
    },
    toggle_record_cmd: () => {
      state.recordStarted = state.recordStarted ? null : Date.now();
      emit("state", state);
    },
    check_update: () => new Promise((r) => setTimeout(() => r({ version: "0.4.0", notes: "**Новое**\n- **Перетаскивание** клипа в Discord\n- Сжатие под лимит `Discord`\n  - 10, 20, 50 или 500 МБ\n\n**Исправлено**\n- Папка открывается поверх оверлея", portable: true }), 600)),
    take_whats_new: () => null,
    copy_image: () => {},
    delete_clips: ({ paths }) => {
      const gone = clips.filter((c) => paths.includes(c.path));
      const bytes = gone.reduce((a, c) => a + c.size, 0);
      for (const c of gone) clips.splice(clips.indexOf(c), 1);
      trashBin[++trashSeq] = gone;
      emit("clips-changed");
      return { batch: trashSeq, count: gone.length, bytes, errors: [] };
    },
    undo_delete: ({ batch }) => {
      const back = trashBin[batch] || [];
      delete trashBin[batch];
      clips.push(...back);
      emit("clips-changed");
      return back.map((c) => c.path);
    },
    audio_levels: () => [{ kind: "system", peak: Math.random() * 0.4, zeroSecs: 0, runningSecs: 100 }, { kind: "mic", peak: Math.random() * 0.25, zeroSecs: 0, runningSecs: 100 }],
    set_favorite: ({ path, on }) => { find(path).favorite = on; },
    clean_preview: ({ maxGb }) => ({ count: maxGb && maxGb < 5 ? 3 : 0, bytes: 2.1e9 }),
    install_update: () => new Promise((r) => {
      let done = 0;
      const t = setInterval(() => {
        done += 1.5e6;
        emit("update-progress", { done: Math.min(done, 11e6), total: 11e6 });
        if (done >= 11e6) { clearInterval(t); r(); }
      }, 120);
    }),
    list_clips: () => clips.slice().sort((a, b) => b.modified - a.modified),
    list_monitors: () => [{ index: 0, name: "DISPLAY1", width: 2560, height: 1440 }],
    list_mics: () => [
      { id: "mic-1", name: "Микрофон (USB Audio)", isDefault: true, muted: false, volume: 0.27, peak: Math.random() * 0.3 },
      { id: "mic-2", name: "Микрофон (Realtek)", isDefault: false, muted: false, volume: 1, peak: 0 },
    ],
    list_audio_apps: () => [{ exe: "Discord.exe", name: "Discord" }, { exe: "bf6.exe", name: "Battlefield 6" }, { exe: "Spotify.exe", name: "Spotify" }],
    set_recording: ({ on }) => { state.recording = on; if (!on) state.status = null; emit("state", state); },
    save_replay_cmd: () => {
      if (!state.recording) return;
      const c = mk(`${state.lastGame} ${stamp(Date.now())}`, 0, settings.bufferMinutes * 60, Math.random() * 300);
      c.modified = Date.now();
      clips.push(c);
      emit("clips-changed");
      emit("clip-saved", c.path);
    },
    screenshot: () => {
      const c = mk(`${state.lastGame} ${stamp(Date.now())}`, 0, 0, 30, "shot");
      clips.push(c);
      emit("clips-changed");
      return c;
    },
    trim_clip: async ({ path, start, end, replace, audioKeep, quality }) => {
      if (quality && quality !== "original") {
        for (let i = 1; i <= 10; i++) { await new Promise((r) => setTimeout(r, 80)); emit("export-progress", i / 10); }
      }
      const c = find(path);
      if (!c) throw "файл не найден";
      const tracks = audioKeep ? audioKeep.length : c.audioTracks;
      const names = audioKeep ? audioKeep.map((i) => c.audioNames[i]) : c.audioNames;
      const s = Math.floor(start);
      const markers = c.markers.filter((m) => m >= s && m <= end).map((m) => m - s);
      if (replace) {
        Object.assign(c, { duration: end - s, size: Math.round((end - s) * 2.6e6), markers, modified: Date.now(), audioTracks: tracks, audioNames: names });
        emit("clips-changed");
        return { clip: c, start: s, end };
      }
      const n = mk(`${c.name} (отрезок)`, 0, end - s, c.hue, "video", { seed: c.seed, markers, audioTracks: tracks, audioNames: names });
      clips.push(n);
      emit("clips-changed");
      return { clip: n, start: s, end };
    },
    rename_clip: ({ path, name }) => {
      const c = find(path);
      if (clips.some((x) => x !== c && x.name === name)) throw "файл с таким именем уже есть";
      c.name = name;
      emit("clips-changed");
      return c;
    },
    delete_clip: ({ path }) => { clips.splice(clips.indexOf(find(path)), 1); emit("clips-changed"); },
    save_frame: ({ label }) => {
      const c = mk(`${label || "Кадр"} ${stamp(Date.now())}`, 0, 0, 30, "shot");
      clips.push(c);
      emit("clips-changed");
      return c;
    },
    set_settings: ({ settings: s }) => { Object.assign(settings, s); emit("state", state); return settings; },
    pick_folder: () => null,
    hide_overlay_cmd: () => { document.body.classList.add("closed"); document.documentElement.classList.remove("overlay-open"); },
    show_main_cmd: () => {},
    reveal_clip: () => {}, open_clips_folder: () => {}, copy_clip: () => {}, frontend_log: () => {}, quit_app: () => {},
  };

  window.api = {
    real: false,
    call: async (cmd, args = {}) => {
      const h = handlers[cmd];
      if (!h) throw `нет команды ${cmd}`;
      await new Promise((r) => setTimeout(r, 30));
      return h(args);
    },
    on: async (event, fn) => { (listeners[event] = listeners[event] || []).push(fn); },
    fileUrl: () => "",
    emit,
  };

  // ---------- нарисованная «игра» для демо ----------
  function lcg(seed) {
    let s = seed >>> 0;
    return () => ((s = (s * 1664525 + 1013904223) >>> 0) / 4294967296);
  }

  window.api.drawDemo = function (ctx, w, h, t, clip, hud = true) {
    const hue = clip.hue;
    const sky = ctx.createLinearGradient(0, 0, 0, h);
    sky.addColorStop(0, `hsl(${hue + 180}, 30%, ${18 + 6 * Math.sin(t / 40)}%)`);
    sky.addColorStop(1, `hsl(${hue}, 35%, 42%)`);
    ctx.fillStyle = sky;
    ctx.fillRect(0, 0, w, h);
    ctx.fillStyle = `hsl(${hue + 160}, 18%, 22%)`;
    ctx.beginPath();
    ctx.moveTo(0, h);
    for (let x = 0; x <= w; x += w / 24) {
      ctx.lineTo(x, h * 0.55 + Math.sin((x / w) * 7 + clip.seed + t * 0.02) * h * 0.06 + Math.sin((x / w) * 17 + clip.seed) * h * 0.03);
    }
    ctx.lineTo(w, h);
    ctx.fill();
    ctx.fillStyle = `hsl(${hue}, 25%, 16%)`;
    ctx.fillRect(0, h * 0.72, w, h * 0.28);
    const pan = (t * 18) % (w * 2);
    const r = lcg(clip.seed);
    for (let i = 0; i < 9; i++) {
      const bw = w * (0.05 + r() * 0.08), bh = h * (0.1 + r() * 0.25);
      const bx = ((r() * w * 2 - pan) % (w * 2) + w * 2) % (w * 2) - w * 0.3;
      ctx.fillStyle = `hsl(${hue + 190}, 10%, ${10 + r() * 10}%)`;
      ctx.fillRect(bx, h * 0.72 - bh, bw, bh);
    }
    if (Math.sin(t * 1.7 + clip.seed) > 0.6) {
      ctx.strokeStyle = "rgba(255,220,140,.85)";
      ctx.lineWidth = Math.max(1, w / 500);
      for (let i = 0; i < 3; i++) {
        const y = h * (0.35 + 0.1 * Math.sin(t * 3 + i));
        const x = ((t * 400 + i * 120) % (w * 1.2)) - w * 0.1;
        ctx.beginPath(); ctx.moveTo(x, y); ctx.lineTo(x + w * 0.06, y - h * 0.01); ctx.stroke();
      }
    }
    const m = (clip.markers || []).find((m) => Math.abs(m - t) < 1.2);
    if (m !== undefined) {
      const k = 1 - Math.abs(m - t) / 1.2;
      const g = ctx.createRadialGradient(w * 0.6, h * 0.55, 0, w * 0.6, h * 0.55, w * 0.25 * k + 1);
      g.addColorStop(0, `rgba(255,200,120,${0.9 * k})`);
      g.addColorStop(1, "rgba(255,120,40,0)");
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, w, h);
    }
    if (!hud) return;
    const u = w / 100;
    ctx.strokeStyle = "rgba(255,255,255,.85)";
    ctx.lineWidth = Math.max(1, u * 0.15);
    ctx.beginPath();
    ctx.moveTo(w / 2 - u * 1.4, h / 2); ctx.lineTo(w / 2 - u * 0.5, h / 2);
    ctx.moveTo(w / 2 + u * 0.5, h / 2); ctx.lineTo(w / 2 + u * 1.4, h / 2);
    ctx.moveTo(w / 2, h / 2 - u * 1.4); ctx.lineTo(w / 2, h / 2 - u * 0.5);
    ctx.moveTo(w / 2, h / 2 + u * 0.5); ctx.lineTo(w / 2, h / 2 + u * 1.4);
    ctx.stroke();
  };
})();
