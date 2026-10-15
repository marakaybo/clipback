// Clipback — интерфейс: главное окно, оверлей и окно-уведомление.

const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const MODE = document.documentElement.dataset.mode; // window | overlay | toast | browser

// ---------- утилиты ----------
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
// пути в Windows не различают регистр
const samePath = (a, b) => !!a && !!b && a.toLowerCase() === b.toLowerCase();

function fmt(t, dec = 2) {
  t = Math.max(0, t || 0);
  const m = Math.floor(t / 60);
  const s = t - m * 60;
  const ss = dec ? s.toFixed(dec).padStart(3 + dec, "0") : String(Math.floor(s)).padStart(2, "0");
  return `${m}:${ss}`;
}

function mb(bytes) {
  const m = bytes / 1048576;
  if (m >= 1024) return `${(m / 1024).toFixed(1)} ГБ`;
  if (m >= 10) return `${Math.round(m)} МБ`;
  return `${m.toFixed(1)} МБ`;
}

function plural(n, [one, few, many]) {
  const a = n % 10, b = n % 100;
  if (a === 1 && b !== 11) return one;
  if (a >= 2 && a <= 4 && (b < 12 || b > 14)) return few;
  return many;
}

const MONTHS = ["января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря"];

function dayLabel(ms) {
  const d = new Date(ms);
  const today = new Date();
  const start = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
  if (ms >= start) return "Сегодня";
  if (ms >= start - 864e5) return "Вчера";
  const y = d.getFullYear() === today.getFullYear() ? "" : ` ${d.getFullYear()}`;
  return `${d.getDate()} ${MONTHS[d.getMonth()]}${y}`;
}

function hhmm(ms) {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

/// «Battlefield 6 2026-10-15 21-48-03» -> { game: "Battlefield 6", when: ms }
function parseName(name) {
  // после даты могут идти приписки: « (2)», « (отрезок)», « (отрезок) (2)»
  const m = /^(.*?) ?(\d{4})-(\d{2})-(\d{2}) (\d{2})-(\d{2})-(\d{2})((?: \([^)]*\))*)$/.exec(name);
  if (!m) return { game: null, when: null, cut: /\(отрезок\)/.test(name) };
  const when = new Date(+m[2], +m[3] - 1, +m[4], +m[5], +m[6], +m[7]).getTime();
  return { game: m[1] && m[1] !== "Clipback" ? m[1] : null, when, cut: /\(отрезок\)/.test(m[8]) };
}

function clipTitle(c) {
  const p = parseName(c.name);
  if (p.when === null) return c.name;
  const base = p.game || (c.kind === "shot" ? "Скриншот" : "Повтор");
  return p.cut ? `${base} — отрезок` : base;
}

const ICONS = {
  replay: `<path d="M3 12a9 9 0 1 0 3-6.7"/><path d="M3 4v5h5"/>`,
  camera: `<path d="M4 8h3l2-3h6l2 3h3v11H4z"/><circle cx="12" cy="13" r="3.5"/>`,
  scissors: `<circle cx="6" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M20 4L8.1 15.9M14.5 14.5L20 20M8.1 8.1L12 12"/>`,
  copy: `<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V5a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h3"/>`,
  trash: `<path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/>`,
  folder: `<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>`,
  info: `<circle cx="12" cy="12" r="9"/><path d="M12 8h.01M11 12h1v5h1"/>`,
  error: `<circle cx="12" cy="12" r="9"/><path d="M12 8v5M12 16h.01"/>`,
  rec: `<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="4" fill="currentColor" stroke="none"/>`,
  check: `<path d="M5 12l4 4L19 6"/>`,
};
const svg = (name) => `<svg viewBox="0 0 24 24">${ICONS[name] || ICONS.info}</svg>`;

// ---------- окно-уведомление ----------
if (MODE === "toast") {
  const el = $("#toast-solo");
  api.on("toast", (p) => {
    $("#toast-solo-title").textContent = p.title;
    $("#toast-solo-sub").textContent = p.sub;
    $("#toast-solo-icon").innerHTML = svg(p.icon);
    el.classList.toggle("is-error", p.icon === "error");
    el.classList.remove("show");
    void el.offsetWidth; // перезапуск анимации
    el.classList.add("show");
  });
} else {
  main();
}

function main() {
  // ---------- состояние ----------
  const S = {
    state: null,
    clips: [],
    clip: null,
    filter: "all",
    t: 0,
    inT: 0,
    outT: 0,
    playing: false,
    loopSel: false,
    speed: 1,
    saveMode: "copy",
    busy: false,
    monitors: [],
    // только что сохранённый повтор — выделим его при следующем обновлении списка
    pendingSelect: null,
    // включённые звуковые дорожки для сохранения отрезка: { номер: да/нет }
    trackOn: {},
    mics: null,
    apps: null,
    exportQuality: "original",
  };

  // ---------- уведомления в окне ----------
  function toast(title, sub = "", icon = "info") {
    const t = document.createElement("div");
    t.className = "toast" + (icon === "error" ? " is-error" : "");
    t.innerHTML = `<div class="ticon">${svg(icon)}</div><div class="toast-text"><b></b><small></small></div>`;
    t.querySelector("b").textContent = title;
    t.querySelector("small").textContent = sub;
    $("#toasts").append(t);
    setTimeout(() => {
      t.classList.add("out");
      setTimeout(() => t.remove(), 200);
    }, icon === "error" ? 5000 : 2800);
  }

  async function call(cmd, args, errTitle) {
    try {
      return await api.call(cmd, args);
    } catch (e) {
      if (errTitle) toast(errTitle, String(e), "error");
      throw e;
    }
  }

  // ---------- подтверждение ----------
  let confirmResolve = null;
  window.confirmBox = confirmBox;
  function confirmBox(title, text, yes = "Да") {
    $("#confirm-title").textContent = title;
    $("#confirm-text").textContent = text;
    $("#confirm-yes").textContent = yes;
    $("#confirm").classList.add("open");
    setTimeout(() => $("#confirm-yes").focus(), 0);
    return new Promise((r) => (confirmResolve = r));
  }
  function closeConfirm(v) {
    $("#confirm").classList.remove("open");
    if (confirmResolve) confirmResolve(v);
    confirmResolve = null;
  }
  $("#confirm-yes").onclick = () => closeConfirm(true);
  $("#confirm-no").onclick = () => closeConfirm(false);
  $("#confirm").addEventListener("pointerdown", (e) => e.target.id === "confirm" && closeConfirm(false));

  // ---------- полноценная запись ----------
  let recTimer = null;
  function renderRecord() {
    const st = S.state;
    const on = !!(st && st.recordStarted);
    const btn = $("#btn-record");
    btn.classList.toggle("recording", on);
    const key = st ? st.settings.hotkeyRecord : "";
    btn.title = on ? `Остановить запись (${key})` : `Начать полноценную запись (${key})`;
    const tick = () => {
      const secs = Math.max(0, (Date.now() - S.state.recordStarted) / 1000);
      $("#record-label").textContent = fmtClock(secs);
    };
    if (on) {
      tick();
      if (!recTimer) recTimer = setInterval(() => (S.state && S.state.recordStarted ? tick() : renderRecord()), 500);
    } else {
      clearInterval(recTimer);
      recTimer = null;
      $("#record-label").textContent = "Запись";
    }
  }
  const fmtClock = (s) => {
    s = Math.floor(s);
    const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, sec = s % 60;
    return (h ? `${h}:${String(m).padStart(2, "0")}` : `${m}`) + `:${String(sec).padStart(2, "0")}`;
  };
  $("#btn-record").onclick = () => call("toggle_record_cmd");

  // ---------- статус записи ----------
  function renderState() {
    const st = S.state;
    if (!st) return;
    const s = st.settings;
    $("#rec-switch").checked = st.recording || st.starting;
    $("#set-recording").checked = st.recording || st.starting;

    let main;
    if (st.starting) main = "Запуск записи…";
    else if (!st.recording) main = "Запись выключена";
    else if (st.status) main = `В буфере <b>${fmt(st.status.bufferedSecs, 0)}</b> из ${fmt(s.bufferMinutes * 60, 0)}`;
    else main = "Запись идёт";
    $("#status-main").innerHTML = main;
    $("#status").classList.toggle("off", !st.recording && !st.starting);
    $("#source-label").textContent = st.capture ? `${st.capture.width}×${st.capture.height} · ${st.capture.fps} к/с` : "";
    $("#btn-save-replay").disabled = !st.recording;
    $("#btn-save-replay").title = `Сохранить последние ${fmt(s.bufferMinutes * 60, 0)} (${s.hotkeySave})`;
    $("#btn-screenshot-live").title = `Скриншот экрана (${s.hotkeyScreenshot})`;
    renderRecord();
    $("#empty-hint").textContent = `Нажмите ${s.hotkeySave} в игре — сохранятся последние ${fmtMinutes(s.bufferMinutes)}. Открыть Clipback поверх игры — ${s.hotkeyOverlay}.`;

    // плашка с проблемой
    const banner = $("#banner");
    const action = $("#banner-action");
    if (st.error) {
      banner.hidden = false;
      $("#banner-text").textContent = `Запись не работает: ${st.error}`;
      action.hidden = false;
      action.textContent = "Повторить";
      action.onclick = () => call("set_recording", { on: true });
    } else if (st.hotkeyErrors && st.hotkeyErrors.length) {
      banner.hidden = false;
      $("#banner-text").textContent = `Клавиша не работает: ${st.hotkeyErrors.join("; ")}`;
      action.hidden = false;
      action.textContent = "Изменить";
      action.onclick = () => openSettings("hotkeys");
    } else {
      banner.hidden = true;
    }
    if ($("#settings").classList.contains("open")) fillSettings();
  }

  function fmtMinutes(m) {
    if (m < 1) return `${Math.round(m * 60)} секунд`;
    const n = Math.round(m * 10) / 10;
    return Number.isInteger(n) ? `${n} ${plural(n, ["минута", "минуты", "минут"])}` : `${n} минуты`;
  }

  $("#rec-switch").addEventListener("change", (e) => call("set_recording", { on: e.target.checked }, "Не удалось"));
  $("#set-recording").addEventListener("change", (e) => call("set_recording", { on: e.target.checked }, "Не удалось"));
  $("#btn-save-replay").onclick = () => call("save_replay_cmd", {}, "Не удалось сохранить");
  $("#btn-screenshot-live").onclick = async () => {
    try {
      const e = await call("screenshot", {});
      toast("Скриншот сохранён", e.name, "camera");
    } catch (e) {
      toast("Скриншот не получился", String(e), "error");
    }
  };
  $("#btn-open-folder").onclick = () => call("open_clips_folder", {}, "Не удалось открыть папку");
  $("#btn-close").onclick = () => call("hide_overlay_cmd");
  $("#btn-to-window").onclick = () => call("show_main_cmd");

  // ---------- список клипов ----------
  const visible = () => S.clips.filter((c) => S.filter === "all" || c.kind === S.filter);

  async function loadClips(selectPath) {
    const list = await call("list_clips", {}, "Не удалось прочитать папку клипов");
    S.clips = list;
    // новый повтор выделяем, только если пользователь сейчас ничего не смотрит и не режет
    if (!selectPath && S.pendingSelect && !S.playing && !S.busy && list.some((c) => samePath(c.path, S.pendingSelect))) {
      selectPath = S.pendingSelect;
    }
    if (list.some((c) => samePath(c.path, S.pendingSelect))) S.pendingSelect = null;
    const keep = selectPath || (S.clip && S.clip.path);
    const next = list.find((c) => samePath(c.path, keep)) || visible()[0] || list[0] || null;
    if (next && S.clip && samePath(next.path, S.clip.path) && !selectPath) {
      // тот же клип: обновляем данные, не сбрасывая плеер (если файл не менялся)
      const changed = next.modified !== S.clip.modified || next.duration !== S.clip.duration;
      S.clip = next;
      if (changed) selectClip(next, true);
      else renderClipList();
    } else {
      selectClip(next, true);
    }
    renderFooter();
  }

  function renderFooter() {
    const n = S.clips.length;
    $("#storage-count").textContent = `${n} ${plural(n, ["файл", "файла", "файлов"])}`;
    $("#storage-used").textContent = mb(S.clips.reduce((s, c) => s + c.size, 0));
  }

  const thumbObserver = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        thumbObserver.unobserve(e.target);
        fillListThumb(e.target);
      }
    },
    { root: $("#clip-list"), rootMargin: "200px" }
  );

  function renderClipList() {
    const box = $("#clip-list");
    box.innerHTML = "";
    const list = visible();
    if (!list.length) {
      const e = document.createElement("div");
      e.className = "list-empty";
      e.textContent = S.clips.length ? "Здесь пусто — смените фильтр" : "Сохранённые клипы появятся здесь";
      box.append(e);
      return;
    }
    let group = null;
    for (const c of list) {
      const g = dayLabel(parseName(c.name).when ?? c.modified);
      if (g !== group) {
        group = g;
        const h = document.createElement("div");
        h.className = "clip-group";
        h.textContent = g;
        box.append(h);
      }
      const el = document.createElement("button");
      el.className = "clip" + (S.clip && c.path === S.clip.path ? " active" : "");
      el.dataset.path = c.path;
      const th = document.createElement("div");
      th.className = "thumb";
      if (c.kind === "shot") th.insertAdjacentHTML("beforeend", `<span class="dur">PNG</span>`);
      else if (c.broken) th.insertAdjacentHTML("beforeend", `<span class="dur warn">ошибка</span>`);
      else th.insertAdjacentHTML("beforeend", `<span class="dur">${fmt(c.duration, 0)}</span>`);
      if (/\(отрезок\)/.test(c.name)) th.insertAdjacentHTML("beforeend", `<span class="badge" title="Отрезок">${svg("scissors")}</span>`);
      const info = document.createElement("div");
      info.className = "clip-info";
      info.innerHTML = `<span class="clip-name"></span><span class="clip-sub"></span>`;
      info.children[0].textContent = clipTitle(c);
      const when = parseName(c.name).when ?? c.modified;
      info.children[1].textContent = `${hhmm(when)} · ${c.kind === "shot" ? `${c.width}×${c.height}` : mb(c.size)}`;
      el.title = c.name;
      el.append(th, info);
      el.onclick = () => selectClip(c);
      box.append(el);
      thumbObserver.observe(el);
    }
    const act = box.querySelector(".clip.active");
    if (act) act.scrollIntoView({ block: "nearest" });
  }

  function fillListThumb(el) {
    const c = S.clips.find((x) => x.path === el.dataset.path);
    const th = el.querySelector(".thumb");
    if (!c || !th || th.querySelector("canvas, img")) return;
    if (c.demo) {
      const cv = document.createElement("canvas");
      cv.width = 176; cv.height = 99;
      api.drawDemo(cv.getContext("2d"), 176, 99, c.kind === "shot" ? 10 : Math.max(0, c.duration - 3), c, false);
      th.prepend(cv);
    } else if (c.kind === "shot") {
      const img = new Image();
      img.src = api.fileUrl(c.path);
      img.loading = "lazy";
      th.prepend(img);
    } else if (!c.broken) {
      thumbs.get(c, Math.max(0, c.duration - 2), 176, 99, "list").then((cv) => cv && th.prepend(cv));
    }
  }

  $$("#clip-filter .seg-btn").forEach((b) =>
    b.addEventListener("click", () => {
      $$("#clip-filter .seg-btn").forEach((x) => x.classList.toggle("active", x === b));
      S.filter = b.dataset.f;
      renderClipList();
    })
  );

  // ---------- превью кадров ----------
  // Отдельный скрытый плеер: перематывает на нужные моменты и рисует кадры.
  const thumbs = (() => {
    const v = document.createElement("video");
    v.muted = true;
    v.preload = "auto";
    v.crossOrigin = "anonymous";
    const queue = [];
    const cache = new Map();
    let busy = false;
    let currentUrl = "";
    let currentPath = "";

    const once = (ev, ms) =>
      new Promise((res, rej) => {
        const t = setTimeout(() => { v.removeEventListener(ev, ok); rej(new Error("timeout")); }, ms);
        const ok = () => { clearTimeout(t); res(); };
        v.addEventListener(ev, ok, { once: true });
      });

    async function pump() {
      if (busy) return;
      busy = true;
      while (queue.length) {
        const job = queue.shift();
        try {
          const url = mediaUrl(job.clip);
          if (currentUrl !== url) {
            currentUrl = url;
            currentPath = job.clip.path;
            v.src = url;
            await once("loadeddata", 8000);
          }
          const t = clamp(job.time, 0, Math.max(0, (v.duration || job.clip.duration) - 0.05));
          if (Math.abs(v.currentTime - t) > 0.01) {
            v.currentTime = t;
            await once("seeked", 5000);
          }
          const cv = document.createElement("canvas");
          cv.width = job.w; cv.height = job.h;
          cv.getContext("2d").drawImage(v, 0, 0, job.w, job.h);
          cache.set(job.key, cv);
          job.resolve(cloneCanvas(cv));
        } catch {
          currentUrl = "";
          job.resolve(null);
        }
      }
      busy = false;
    }

    return {
      get(clip, time, w, h, group) {
        const key = `${clip.path}|${clip.modified}|${time.toFixed(2)}|${w}`;
        if (cache.has(key)) return Promise.resolve(cloneCanvas(cache.get(key)));
        return new Promise((resolve) => {
          const job = { clip, time, w, h, key, group, resolve };
          // кадры для таймлайна важнее превью в списке
          if (group === "timeline") queue.unshift(job);
          else queue.push(job);
          pump();
        });
      },
      cancel(group) {
        for (let i = queue.length - 1; i >= 0; i--) {
          if (queue[i].group === group) queue.splice(i, 1)[0].resolve(null);
        }
      },
      forget(path) {
        for (const k of cache.keys()) if (k.startsWith(path + "|")) cache.delete(k);
        if (currentPath === path) {
          currentUrl = "";
          currentPath = "";
          v.removeAttribute("src");
          v.load();
        }
      },
    };
  })();

  function cloneCanvas(src) {
    const c = document.createElement("canvas");
    c.width = src.width; c.height = src.height;
    c.getContext("2d").drawImage(src, 0, 0);
    return c;
  }

  // ---------- плеер ----------
  const video = $("#video-el");
  const image = $("#image-el");
  const vc = $("#video-canvas");
  const vctx = vc.getContext("2d");
  const stage = $("#stage");
  video.crossOrigin = "anonymous";

  function mediaUrl(c) {
    // ?v= — чтобы после обрезки «на месте» плеер не показал старую версию файла
    return `${api.fileUrl(c.path)}?v=${c.modified}`;
  }

  function releaseMedia() {
    setPlaying(false);
    video.removeAttribute("src");
    video.load();
  }

  function selectClip(c, fromReload = false) {
    const same = S.clip && c && S.clip.path === c.path && S.clip.modified === c.modified;
    S.clip = c;
    setPlaying(false);
    $("#app").classList.toggle("no-clip", !c);
    $("#app").classList.toggle("is-shot", !!c && c.kind === "shot");
    $("#stage-msg").hidden = true;
    renderClipList();
    if (!c) {
      releaseMedia();
      $("#clip-title").textContent = "";
      $("#clip-meta").textContent = "";
      return;
    }

    $("#clip-title").textContent = clipTitle(c);
    $("#clip-title").dataset.name = c.name;
    const when = parseName(c.name).when ?? c.modified;
    $("#clip-meta").textContent =
      c.kind === "shot"
        ? `${dayLabel(when)}, ${hhmm(when)} · ${c.width}×${c.height} · ${mb(c.size)}`
        : `${dayLabel(when)}, ${hhmm(when)} · ${c.width}×${c.height} · ${Math.round(c.fps)} к/с · ${audioLabel(c)} · ${mb(c.size)}`;

    stage.classList.toggle("demo", !!c.demo);
    stage.classList.toggle("show-video", !c.demo && c.kind === "video");
    stage.classList.toggle("show-image", !c.demo && c.kind === "shot");

    if (c.kind === "shot") {
      releaseMedia();
      if (!c.demo) image.src = api.fileUrl(c.path);
      else api.drawDemo(vctx, vc.width, vc.height, 10, c);
      return;
    }
    if (c.broken) {
      releaseMedia();
      $("#stage-msg").hidden = false;
      $("#stage-msg").textContent = "Файл не читается: он повреждён или это не MP4 от Clipback.";
      return;
    }

    if (!same) S.trackOn = {};
    renderQuality();
    // по умолчанию — последние 30 секунд: в повторе нужный момент обычно в конце
    if (!same || !fromReload) {
      S.outT = c.duration;
      S.inT = Math.max(0, c.duration - 30);
      S.t = S.inT;
    }
    if (!c.demo) {
      const url = mediaUrl(c);
      if (video.getAttribute("src") !== url) {
        video.src = url;
      }
      video.playbackRate = S.speed;
      video.currentTime = S.t;
    }
    $("#tc-dur").textContent = fmt(c.duration);
    buildTimeline();
    updateAll();
  }

  video.addEventListener("error", () => {
    if (!video.getAttribute("src") || !S.clip || S.clip.demo) return;
    $("#stage-msg").hidden = false;
    $("#stage-msg").textContent = "Не удалось открыть видео.";
    api.call("frontend_log", { message: `video error ${video.error && video.error.code}: ${S.clip.path}` }).catch(() => {});
  });
  video.addEventListener("ended", () => setPlaying(false));

  function setPlaying(p) {
    const c = S.clip;
    if (!c || c.kind !== "video" || c.broken) p = false;
    if (p && S.t >= S.outT - 0.05 && S.t <= S.outT + 0.05) seek(S.inT);
    if (p && S.t >= (c ? c.duration : 0) - 0.05) seek(S.inT);
    S.playing = p;
    if (p) S.loopSel = S.t >= S.inT - 0.05 && S.t < S.outT;
    if (c && !c.demo) {
      if (p) video.play().catch(() => { S.playing = false; renderPlay(); });
      else video.pause();
    }
    renderPlay();
  }

  function renderPlay() {
    stage.classList.toggle("playing", S.playing);
    $("#play-icon").innerHTML = S.playing ? `<path d="M7 5h4v14H7zM13 5h4v14h-4z"/>` : `<path d="M8 5v14l11-7z"/>`;
  }

  function seek(t, fromUser = false) {
    if (!S.clip) return;
    S.t = clamp(t, 0, S.clip.duration);
    if (fromUser && (S.t < S.inT || S.t > S.outT)) S.loopSel = false;
    if (!S.clip.demo && S.clip.kind === "video") video.currentTime = S.t;
    updateAll();
  }

  $("#btn-play").onclick = () => setPlaying(!S.playing);
  $("#big-play").onclick = () => setPlaying(true);
  stage.addEventListener("click", (e) => {
    if (e.target === video || e.target === vc) setPlaying(!S.playing);
  });
  // Во весь экран: плеер растягивается на всё окно; главное окно при этом уходит в полноэкранный режим
  // (оверлей и так закрывает весь экран).
  const fullIcon = {
    on: '<path d="M9 4v5H4M15 4v5h5M15 20v-5h5M9 20v-5H4"/>',
    off: '<path d="M4 9V4h5M15 4h5v5M20 15v5h-5M9 20H4v-5"/>',
  };
  function setFullView(on) {
    if (on && document.querySelector(".app").classList.contains("no-clip")) return;
    document.body.classList.toggle("fullview", on);
    $("#full-icon").innerHTML = on ? fullIcon.on : fullIcon.off;
    $("#btn-full").title = on ? "Выйти из полноэкранного (F, Esc)" : "Во весь экран (F)";
    if (MODE === "window" && window.__TAURI__) {
      window.__TAURI__.window.getCurrentWindow().setFullscreen(on).catch(() => {});
    }
  }
  const isFullView = () => document.body.classList.contains("fullview");
  $("#btn-full").onclick = () => setFullView(!isFullView());
  stage.addEventListener("dblclick", (e) => {
    if (e.target === video || e.target === vc) setFullView(!isFullView());
  });
  $("#btn-back").onclick = () => seek(S.t - 5, true);
  $("#btn-fwd").onclick = () => seek(S.t + 5, true);
  $$("#speed .seg-btn").forEach((b) =>
    b.addEventListener("click", () => {
      $$("#speed .seg-btn").forEach((x) => x.classList.toggle("active", x === b));
      S.speed = +b.dataset.s;
      video.playbackRate = S.speed;
    })
  );

  // громкость
  const savedVol = +(localStorageGet("clipback.volume") ?? 80);
  $("#vol").value = savedVol;
  video.volume = savedVol / 100;
  $("#vol").addEventListener("input", (e) => {
    video.volume = e.target.value / 100;
    video.muted = false;
    renderVolume();
    localStorageSet("clipback.volume", e.target.value);
  });
  $("#btn-mute").onclick = () => { video.muted = !video.muted; renderVolume(); };
  function renderVolume() {
    const off = video.muted || video.volume === 0;
    $("#vol-icon").innerHTML = off
      ? `<path d="M4 9h4l5-4v14l-5-4H4z"/><path d="M17 9l4 6M21 9l-4 6"/>`
      : `<path d="M4 9h4l5-4v14l-5-4H4z"/><path d="M16 9a4 4 0 0 1 0 6"/>`;
  }

  function localStorageGet(k) { try { return localStorage.getItem(k); } catch { return null; } }
  function localStorageSet(k, v) { try { localStorage.setItem(k, v); } catch {} }

  // ---------- таймлайн ----------
  const tl = $("#tl");
  const tlW = () => tl.clientWidth;
  const tToX = (t) => (t / (S.clip && S.clip.duration ? S.clip.duration : 1)) * tlW();
  const xToT = (x) => clamp((x / tlW()) * (S.clip ? S.clip.duration : 0), 0, S.clip ? S.clip.duration : 0);

  let tlToken = 0;
  function buildTimeline() {
    const c = S.clip;
    const w = tlW();
    if (!c || c.kind !== "video" || !w) return;
    const token = ++tlToken;

    const ruler = $("#ruler");
    ruler.innerHTML = "";
    const steps = [1, 2, 5, 10, 15, 30, 60, 120, 300];
    const step = steps.find((s) => (c.duration / s) * 70 <= w) || 600;
    for (let t = 0; t <= c.duration - step / 2; t += step) {
      const d = document.createElement("div");
      d.className = "tick";
      d.style.left = `${tToX(t)}px`;
      d.textContent = fmt(t, 0);
      if (t === 0) d.style.transform = "none";
      ruler.append(d);
    }

    const box = $("#thumbs");
    box.innerHTML = "";
    thumbs.cancel("timeline");
    const n = clamp(Math.floor(w / 72), 4, 24);
    for (let i = 0; i < n; i++) {
      const slot = document.createElement("div");
      slot.className = "tslot";
      box.append(slot);
      const t = ((i + 0.5) / n) * c.duration;
      if (c.demo) {
        const cv = document.createElement("canvas");
        cv.width = 128; cv.height = 72;
        api.drawDemo(cv.getContext("2d"), 128, 72, t, c, false);
        slot.append(cv);
      } else {
        thumbs.get(c, t, 128, 72, "timeline").then((cv) => cv && token === tlToken && slot.append(cv));
      }
    }

    $$(".marker", tl).forEach((m) => m.remove());
    for (const m of c.markers || []) {
      const d = document.createElement("div");
      d.className = "marker";
      d.style.left = `${tToX(m)}px`;
      tl.append(d);
    }
    renderTracks();
  }

  new ResizeObserver(() => {
    resizeCanvas();
    buildTimeline();
    updateAll();
  }).observe(tl);

  function updateTimeline() {
    if (!S.clip || S.clip.kind !== "video") return;
    const w = tlW();
    const xi = tToX(S.inT), xo = tToX(S.outT);
    $("#dim-left").style.width = `${xi}px`;
    $("#dim-right").style.width = `${Math.max(0, w - xo)}px`;
    const sel = $("#sel");
    sel.style.left = `${xi}px`;
    sel.style.width = `${Math.max(0, xo - xi)}px`;
    tl.style.setProperty("--sel-l", `${xi}px`);
    tl.style.setProperty("--sel-r", `${Math.max(0, w - xo)}px`);
    $("#playhead").style.left = `${tToX(S.t)}px`;
  }

  function drag(onMove, cursor) {
    return (e) => {
      if (!S.clip || S.clip.kind !== "video" || e.button !== 0) return;
      e.preventDefault();
      e.stopPropagation();
      const rect = tl.getBoundingClientRect();
      const wasPlaying = S.playing;
      if (wasPlaying) setPlaying(false);
      const move = (ev) => onMove(xToT(ev.clientX - rect.left));
      move(e);
      if (cursor) document.body.style.cursor = cursor;
      const up = () => {
        removeEventListener("pointermove", move);
        removeEventListener("pointerup", up);
        document.body.style.cursor = "";
      };
      addEventListener("pointermove", move);
      addEventListener("pointerup", up);
    };
  }
  const MIN_LEN = 0.5;
  $("#h-in").addEventListener("pointerdown", drag((t) => { S.inT = Math.min(t, S.outT - MIN_LEN); seek(S.inT); }, "ew-resize"));
  $("#h-out").addEventListener("pointerdown", drag((t) => { S.outT = Math.max(t, S.inT + MIN_LEN); seek(S.outT); }, "ew-resize"));
  $(".ph-knob").addEventListener("pointerdown", drag((t) => seek(t, true), "grabbing"));
  tl.addEventListener("pointerdown", drag((t) => seek(t, true)));

  function setIn() { if (!S.clip) return; S.inT = clamp(S.t, 0, S.outT - MIN_LEN); updateAll(); }
  function setOut() { if (!S.clip) return; S.outT = clamp(S.t, S.inT + MIN_LEN, S.clip.duration); updateAll(); }
  $("#btn-in").onclick = setIn;
  $("#btn-out").onclick = setOut;
  $("#btn-last30").onclick = () => { if (!S.clip) return; S.outT = S.clip.duration; S.inT = Math.max(0, S.outT - 30); seek(S.inT); };
  $("#btn-reset").onclick = () => { if (!S.clip) return; S.inT = 0; S.outT = S.clip.duration; updateAll(); };

  // ---------- звуковые дорожки ----------
  // Названия дорожек хранятся в файле. «Всё вместе» (система + микрофон) — для плееров и Discord,
  // в интерфейсе её не показываем: она остаётся в отрезке, только если оставлены и система, и микрофон.
  function trackNames(c) {
    if (!c || !c.audioTracks) return [];
    if (c.audioNames && c.audioNames.length === c.audioTracks) return c.audioNames;
    if (c.audioTracks === 3) return ["Всё вместе", "Звук системы", "Микрофон"];
    if (c.audioTracks === 1) return ["Звук"];
    return Array.from({ length: c.audioTracks }, (_, i) => `Дорожка ${i + 1}`);
  }

  const MIX = "Всё вместе";
  const hasMix = (names) => names[0] === MIX && names.includes("Звук системы") && names.includes("Микрофон");

  /// Дорожки, которые видит пользователь: [{ index, name }].
  function visibleTracks(c) {
    const names = trackNames(c);
    return names.map((name, index) => ({ index, name })).filter((t) => !(t.index === 0 && hasMix(names)));
  }

  function audioLabel(c) {
    const names = trackNames(c);
    if (!names.length) return "без звука";
    const v = visibleTracks(c).map((t) => t.name.toLowerCase());
    return v.length <= 3 ? v.join(" + ") : `${v.length} дорожек звука`;
  }

  const isOn = (i) => S.trackOn[i] !== false;

  /// Какие дорожки оставить в отрезке (номера в файле). null — все.
  function audioKeep(c) {
    const names = trackNames(c);
    const vis = visibleTracks(c);
    if (!names.length || vis.every((t) => isOn(t.index))) return null;
    const keep = vis.filter((t) => isOn(t.index)).map((t) => t.index);
    if (hasMix(names)) {
      const sys = names.indexOf("Звук системы"), mic = names.indexOf("Микрофон");
      if (isOn(sys) && isOn(mic)) keep.unshift(0);
    }
    return keep;
  }

  // форма волны: грузится один раз на клип
  let peaksToken = 0;
  function renderTracks() {
    const c = S.clip;
    const box = $("#tracks");
    box.innerHTML = "";
    if (!c || c.kind !== "video") return;
    const vis = visibleTracks(c);
    const w = tlW();
    for (const t of vis) {
      const row = document.createElement("div");
      row.className = "track" + (isOn(t.index) ? "" : " off");
      row.dataset.index = t.index;
      for (const cls of ["", "hi"]) {
        const cv = document.createElement("canvas");
        cv.width = Math.max(1, Math.round(w * devicePixelRatio));
        cv.height = Math.round(24 * devicePixelRatio);
        if (cls) cv.className = cls;
        row.append(cv);
      }
      const name = document.createElement("label");
      name.className = "tname";
      name.title = "Выключенная дорожка не попадёт в отрезок";
      name.innerHTML = `<span class="switch xs"><input type="checkbox"><span></span></span><span></span>`;
      name.lastChild.textContent = t.name;
      const cb = name.querySelector("input");
      cb.checked = isOn(t.index);
      cb.addEventListener("change", () => {
        S.trackOn[t.index] = cb.checked;
        row.classList.toggle("off", !cb.checked);
        updateExport();
      });
      name.addEventListener("pointerdown", (e) => e.stopPropagation());
      row.append(name);
      box.append(row);
    }
    $("#tracks-hint").hidden = vis.length < 2;
    drawPeaks(c);
  }

  async function drawPeaks(c) {
    const token = ++peaksToken;
    let peaks;
    try {
      peaks = c.demo ? demoPeaks(c) : await api.call("audio_peaks", { path: c.path, bins: 600 });
    } catch {
      return;
    }
    if (token !== peaksToken || S.clip !== c) return;
    for (const row of $$("#tracks .track")) {
      const p = peaks[+row.dataset.index];
      if (!p) continue;
      const [base, hi] = row.querySelectorAll("canvas");
      paintWave(base, p, "#6d717a");
      paintWave(hi, p, "#4da8ff");
    }
  }

  function demoPeaks(c) {
    return trackNames(c).map((_, k) => {
      let s = c.seed + k * 977;
      const r = () => ((s = (s * 1664525 + 1013904223) >>> 0) / 4294967296);
      let talk = 1;
      return Array.from({ length: 600 }, () => {
        if (k > 1 && r() < 0.03) talk = r() < 0.5 ? 0 : 1;
        return (0.15 + r() * 0.6) * talk;
      });
    });
  }

  function paintWave(cv, peaks, color) {
    const ctx = cv.getContext("2d");
    const { width: w, height: h } = cv;
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = color;
    const bar = 2 * devicePixelRatio;
    for (let x = 0; x < w; x += bar + devicePixelRatio) {
      const v = peaks[Math.min(peaks.length - 1, Math.floor((x / w) * peaks.length))] || 0;
      // громкость в дБ: тихое тоже видно
      const db = v > 0 ? 20 * Math.log10(v) : -90;
      const a = clamp((db + 54) / 54, 0, 1);
      const bh = Math.max(devicePixelRatio, a * h * 0.9);
      ctx.fillRect(x, (h - bh) / 2, bar, bh);
    }
  }

  // ---------- сохранение отрезка ----------
  $$("#save-mode .rcard").forEach((b) =>
    b.addEventListener("click", () => {
      $$("#save-mode .rcard").forEach((x) => x.classList.toggle("active", x === b));
      S.saveMode = b.dataset.m;
    })
  );

  // качество отрезка: original — без перекодирования; остальные — как в ядре (media::Quality)
  const QUALITY = {
    high: { maxH: 1440, bpp: 0.06, name: "Высокое" },
    medium: { maxH: 1080, bpp: 0.05, name: "Среднее" },
    low: { maxH: 720, bpp: 0.045, name: "Низкое" },
  };
  S.exportQuality = localStorageGet("clipback.exportQuality") || "original";
  if (S.exportQuality !== "original" && !QUALITY[S.exportQuality]) S.exportQuality = "original";

  function qualityOut(c, q) {
    const p = QUALITY[q];
    let w = c.width, h = c.height;
    if (h > p.maxH) { w = Math.round((w * p.maxH) / h); h = p.maxH; }
    w -= w % 2; h -= h % 2;
    const fps = c.fps > 1 ? c.fps : 60;
    const bitrate = clamp(w * h * fps * p.bpp, 5e5, 8e7);
    return { w, h, bitrate };
  }

  function renderQuality() {
    $$("#export-quality .seg-btn").forEach((b) => b.classList.toggle("active", b.dataset.q === S.exportQuality));
    const c = S.clip;
    const hint = $("#quality-hint");
    if (S.exportQuality === "original" || !c) {
      hint.textContent = "Без перекодирования: мгновенно и без потерь. Начало сдвигается к ближайшему ключевому кадру (до 1 с раньше).";
    } else {
      const o = qualityOut(c, S.exportQuality);
      hint.textContent = `${o.h}p, ≈ ${(o.bitrate / 1e6).toFixed(1)} Мбит/с. Перекодируется на видеокарте, режется точно по кадру.`;
    }
  }

  $$("#export-quality .seg-btn").forEach((b) =>
    b.addEventListener("click", () => {
      S.exportQuality = b.dataset.q;
      localStorageSet("clipback.exportQuality", S.exportQuality);
      renderQuality();
      updateExport();
    })
  );

  function estimateSize(c, len) {
    if (S.exportQuality === "original" || !QUALITY[S.exportQuality]) return c.duration > 0 ? (c.size / c.duration) * len : 0;
    const keep = audioKeep(c);
    const tracks = keep ? keep.length : c.audioTracks || 0;
    return (qualityOut(c, S.exportQuality).bitrate / 8 + tracks * 20000) * len;
  }

  function updateExport() {
    const c = S.clip;
    if (!c || c.kind !== "video") return;
    const len = S.outT - S.inT;
    $("#est-len").textContent = fmt(len, 1);
    $("#est-size").textContent = c.duration > 0 ? mb(estimateSize(c, len)) : "—";
    const whole = S.inT <= 0.01 && S.outT >= c.duration - 0.01 && audioKeep(c) === null && S.exportQuality === "original";
    $("#btn-export").disabled = S.busy || whole;
    $("#btn-export").title = whole ? "Выделите на таймлайне нужный кусок, выключите дорожку или выберите качество" : "Ctrl+S";
  }

  function showProgress(f) {
    const bar = $("#export-progress");
    bar.hidden = f === null;
    if (f !== null) bar.firstElementChild.style.width = `${Math.round(f * 100)}%`;
  }
  api.on("export-progress", (f) => { if (S.busy) showProgress(f); });

  async function exportClip() {
    const c = S.clip;
    if (!c || c.kind !== "video" || S.busy || $("#btn-export").disabled) return;
    const len = S.outT - S.inT;
    const replace = S.saveMode === "replace";
    if (replace && !(await confirmBox("Оставить только отрезок?", `В файле останется ${fmt(len, 1)}, остальное видео удалится.`, "Заменить"))) return;
    S.busy = true;
    updateExport();
    const reencode = S.exportQuality !== "original";
    if (reencode) showProgress(0);
    if (replace) {
      releaseMedia();
      thumbs.forget(c.path);
    }
    try {
      const r = await api.call("trim_clip", {
        path: c.path,
        start: S.inT,
        end: S.outT,
        replace,
        audioKeep: audioKeep(c),
        quality: S.exportQuality,
      });
      const shift = S.inT - r.start;
      const note = shift > 0.05 ? ` · начало на ${shift.toFixed(1)} с раньше (ключевой кадр)` : "";
      const q = reencode ? ` · ${QUALITY[S.exportQuality].name.toLowerCase()} качество, ${mb(r.clip.size)}` : "";
      toast(replace ? "Клип обрезан" : "Отрезок сохранён", `${fmt(r.end - r.start, 1)}${q}${note}`, "scissors");
      await loadClips(r.clip.path);
    } catch (e) {
      toast("Не удалось сохранить отрезок", String(e), "error");
      if (replace) selectClip(c);
    } finally {
      S.busy = false;
      showProgress(null);
      updateExport();
    }
  }
  $("#btn-export").onclick = exportClip;

  async function saveFrame() {
    const c = S.clip;
    if (!c || c.kind !== "video" || c.broken) return;
    const label = clipTitle(c);
    try {
      let png = "";
      if (!c.demo) {
        if (!video.videoWidth) throw "кадр ещё не загрузился";
        const cv = document.createElement("canvas");
        cv.width = video.videoWidth;
        cv.height = video.videoHeight;
        cv.getContext("2d").drawImage(video, 0, 0);
        png = cv.toDataURL("image/png").split(",")[1];
      }
      const e = await api.call("save_frame", { pngBase64: png, label });
      toast("Кадр сохранён", `${fmt(S.t, 2)} · ${e.name}`, "camera");
    } catch (e) {
      toast("Не удалось сохранить кадр", String(e), "error");
    }
  }
  $("#btn-frame").onclick = saveFrame;

  // ---------- редактор скриншота ----------
  async function editShot() {
    const c = S.clip;
    if (!c || c.kind !== "shot" || !window.ImageEditor) return;
    let src;
    try {
      if (c.demo) {
        src = document.createElement("canvas");
        src.width = 1920;
        src.height = 1080;
        api.drawDemo(src.getContext("2d"), src.width, src.height, 10, c);
      } else {
        src = await new Promise((res, rej) => {
          const im = new Image();
          im.crossOrigin = "anonymous";
          im.onload = () => res(im);
          im.onerror = () => rej("картинка не загрузилась");
          im.src = api.fileUrl(c.path) + (api.fileUrl(c.path).includes("?") ? "&" : "?") + "edit=" + c.modified;
        });
      }
    } catch (e) {
      return toast("Не удалось открыть редактор", String(e), "error");
    }
    window.ImageEditor.open(src, {
      title: clipTitle(c),
      onSave: async (png, replace) => {
        try {
          const e = await api.call("save_edited_image", { path: c.path, pngBase64: png, replace });
          toast(replace ? "Скриншот изменён" : "Копия сохранена", e.name, "check");
          S.pendingSelect = e.path;
          await loadClips(e.path);
        } catch (err) {
          toast("Не удалось сохранить", String(err), "error");
          throw err;
        }
      },
    });
  }
  $("#btn-edit").onclick = editShot;

  // ---------- действия с клипом ----------
  $("#btn-copy").onclick = async () => {
    if (!S.clip) return;
    try {
      await api.call("copy_clip", { path: S.clip.path });
      toast("Файл скопирован", "Вставьте его в Discord или Telegram через Ctrl+V", "copy");
    } catch (e) {
      toast("Не удалось скопировать", String(e), "error");
    }
  };
  $("#btn-reveal").onclick = () => S.clip && call("reveal_clip", { path: S.clip.path }, "Не удалось открыть папку");

  async function deleteClip() {
    const c = S.clip;
    if (!c || S.busy) return;
    const list = visible();
    const idx = list.findIndex((x) => x.path === c.path);
    const next = list[idx + 1] || list[idx - 1] || null;
    releaseMedia();
    thumbs.forget(c.path);
    try {
      await api.call("delete_clip", { path: c.path });
      toast("В корзине", `${clipTitle(c)} — можно восстановить из корзины Windows`, "trash");
      S.clip = null;
      await loadClips(next && next.path);
    } catch (e) {
      toast("Не удалось удалить", String(e), "error");
      selectClip(c);
    }
  }
  $("#btn-delete").onclick = deleteClip;

  // переименование: щелчок по названию
  const title = $("#clip-title");
  title.addEventListener("click", () => {
    if (!S.clip || title.isContentEditable) return;
    title.contentEditable = "true";
    title.textContent = S.clip.name;
    title.focus();
    document.getSelection().selectAllChildren(title);
  });
  title.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter") { e.preventDefault(); title.blur(); }
    if (e.key === "Escape") { title.textContent = S.clip ? S.clip.name : ""; title.blur(); }
  });
  title.addEventListener("blur", async () => {
    if (!title.isContentEditable) return;
    title.contentEditable = "false";
    const c = S.clip;
    const name = title.textContent.trim();
    if (!c || !name || name === c.name) {
      if (c) title.textContent = clipTitle(c);
      return;
    }
    try {
      const e = await api.call("rename_clip", { path: c.path, name });
      thumbs.forget(c.path);
      await loadClips(e.path);
    } catch (err) {
      toast("Не удалось переименовать", String(err), "error");
      title.textContent = clipTitle(c);
    }
  });

  // ---------- главный цикл ----------
  function resizeCanvas() {
    const r = stage.getBoundingClientRect();
    const w = Math.min(r.width, (r.height * 16) / 9);
    vc.width = Math.max(320, Math.round(w * devicePixelRatio));
    vc.height = Math.round((vc.width * 9) / 16);
  }

  function updateAll() {
    const c = S.clip;
    $("#tc-cur").textContent = fmt(S.t);
    if (c && c.kind === "video") {
      $("#sel-len").textContent = fmt(S.outT - S.inT, 1);
      $("#sel-range").textContent = `${fmt(S.inT, 1)} — ${fmt(S.outT, 1)}`;
      updateTimeline();
      updateExport();
    }
  }

  let last = performance.now();
  function frame(now) {
    const dt = Math.min(0.1, (now - last) / 1000);
    last = now;
    const c = S.clip;
    if (c && c.kind === "video" && S.playing) {
      if (c.demo) S.t += dt * S.speed;
      else S.t = video.currentTime;
      if (S.loopSel && S.t >= S.outT) seek(S.inT);
      else if (S.t >= c.duration) { S.t = c.duration; setPlaying(false); }
      updateAll();
    }
    if (c && c.demo && c.kind === "video") api.drawDemo(vctx, vc.width, vc.height, S.t, c);
    if (MODE === "browser") drawGameBg(now);
    requestAnimationFrame(frame);
  }

  // ---------- настройки ----------
  function openSettings(tab) {
    fillSettings();
    if (tab) switchTab(tab);
    $("#settings").classList.add("open");
    if (!S.monitors.length) {
      api.call("list_monitors").then((m) => { S.monitors = m; fillSettings(); }).catch(() => {});
    }
    refreshMics();
    refreshApps();
    syncMeter();
  }
  function closeSettings() {
    $("#settings").classList.remove("open");
    stopKeyCapture();
    syncMeter();
  }
  function switchTab(tab) {
    $$(".nav-item").forEach((x) => x.classList.toggle("active", x.dataset.tab === tab));
    $$(".tab").forEach((t) => t.classList.toggle("active", t.dataset.tab === tab));
    syncMeter();
  }
  $("#btn-settings").onclick = () => openSettings();
  $("#settings-close").onclick = closeSettings;
  $("#settings").addEventListener("pointerdown", (e) => e.target.id === "settings" && closeSettings());
  $$(".nav-item").forEach((b) => b.addEventListener("click", () => switchTab(b.dataset.tab)));

  /// Битрейт записи, Мбит/с — как в настройках приложения (settings.rs).
  function bitrate(s) {
    if (s.quality === "custom") return s.customBitrate;
    const capH = S.state && S.state.capture ? S.state.capture.height : 1080;
    const h = s.maxHeight ? Math.min(s.maxHeight, capH) : capH;
    const base = { low: 5, medium: 8, high: 14 }[s.quality] || 8;
    return clamp(Math.round(base * (h / 1080) ** 2 * Math.sqrt(s.fps / 60)), 2, 120);
  }

  const perMinute = (mbps) => mb((mbps * 1e6 / 8) * 60);

  function fillSettings() {
    const st = S.state;
    if (!st) return;
    const s = st.settings;
    $("#set-buffer").value = Math.round(s.bufferMinutes * 2);
    $("#set-buffer-val").textContent = `${s.bufferMinutes} мин`;
    $("#buf-mem").textContent = `Займёт в памяти ≈ ${mb((bitrate(s) * 1e6 / 8) * s.bufferMinutes * 60)}`;
    $("#set-record-on-start").checked = s.recordOnStart;
    $("#set-autostart").checked = s.autostart;
    $("#set-close-to-tray").checked = s.closeToTray;
    $("#set-height").value = s.maxHeight ? String(s.maxHeight) : "";
    $$("#set-fps .seg-btn").forEach((b) => b.classList.toggle("active", +b.dataset.v === s.fps));
    $$("#set-quality .seg-btn").forEach((b) => b.classList.toggle("active", b.dataset.v === s.quality));
    $("#set-bitrate").textContent = `≈ ${bitrate(s)} Мбит/с — около ${perMinute(bitrate(s))} за минуту видео`;
    $("#custom-bitrate-row").hidden = s.quality !== "custom";
    $("#set-custom-bitrate").value = s.customBitrate;
    $("#set-custom-bitrate-val").textContent = `${s.customBitrate} Мбит/с`;
    $("#set-encoder").textContent = st.capture ? st.capture.encoder : "—";
    $("#set-outdir").textContent = s.outDir;

    // звук
    $$("#set-audio-mode .seg-btn").forEach((b) => b.classList.toggle("active", b.dataset.v === s.audioMode));
    $("#audio-mode-hint").textContent =
      s.audioMode === "advanced"
        ? "Звук системы, микрофон и отдельные дорожки выбранных приложений"
        : "Две дорожки: весь звук компьютера и микрофон";
    $("#apps-section").hidden = s.audioMode !== "advanced";
    $("#set-system-audio").checked = s.systemAudio;
    $("#set-mic").checked = s.mic;
    $$(".mic-row").forEach((r) => r.classList.toggle("disabled", !s.mic));
    const micSel = $("#set-mic-device");
    const mics = S.mics || [];
    micSel.innerHTML = `<option value="">Как в Windows</option>` + mics.map(() => `<option></option>`).join("");
    [...micSel.options].slice(1).forEach((o, i) => {
      o.value = mics[i].id;
      o.textContent = mics[i].name + (mics[i].isDefault ? " — основной в Windows" : "");
    });
    micSel.value = s.micDevice || "";
    micSel.disabled = !s.mic;
    $("#set-mic-gain").value = Math.round(s.micGain * 100);
    $("#set-mic-gain-val").textContent = `×${s.micGain.toFixed(2).replace(/\.?0+$/, "")}`;
    renderMicHint();
    renderAppTracks();
    const aerr = st.capture && st.capture.audioError;
    $("#audio-error").hidden = !aerr;
    $("#audio-error").textContent = aerr ? `Звук не пишется: ${aerr}` : "";

    $("#set-check-updates").checked = s.checkUpdates;
    $("#about-kind").textContent = st.portable ? "portable-версия" : "установленная версия";
    renderUpdate();
    $$(".keybind").forEach((k) => { if (!k.classList.contains("listening")) k.textContent = s[k.dataset.key]; });
    const sel = $("#set-monitor");
    const mons = S.monitors.length ? S.monitors : [{ index: s.monitor, name: `Монитор ${s.monitor + 1}`, width: 0, height: 0 }];
    sel.innerHTML = mons.map((m) => `<option value="${m.index}">${m.index + 1}: ${m.name}${m.width ? ` · ${m.width}×${m.height}` : ""}</option>`).join("");
    sel.value = String(s.monitor);
  }

  async function saveSettings(patch) {
    const s = { ...S.state.settings, ...patch };
    try {
      S.state.settings = await api.call("set_settings", { settings: s });
    } catch (e) {
      toast("Не удалось сохранить настройки", String(e), "error");
    }
    renderState();
  }

  $("#set-buffer").addEventListener("input", (e) => {
    const m = e.target.value / 2;
    $("#set-buffer-val").textContent = `${m} мин`;
    $("#buf-mem").textContent = `Займёт в памяти ≈ ${mb((bitrate(S.state.settings) * 1e6 / 8) * m * 60)}`;
  });
  $("#set-buffer").addEventListener("change", (e) => saveSettings({ bufferMinutes: e.target.value / 2 }));
  $("#set-record-on-start").addEventListener("change", (e) => saveSettings({ recordOnStart: e.target.checked }));
  $("#set-autostart").addEventListener("change", (e) => saveSettings({ autostart: e.target.checked }));
  $("#set-close-to-tray").addEventListener("change", (e) => saveSettings({ closeToTray: e.target.checked }));
  $("#set-check-updates").addEventListener("change", (e) => saveSettings({ checkUpdates: e.target.checked }));

  // ---------- обновления ----------
  // upd.state: idle | checking | none | available | loading | error
  // var: fillSettings может вызвать renderUpdate раньше, чем дойдёт до этой строки
  var upd = { state: "idle", info: null, error: "" };
  function renderUpdate() {
    if (!upd) return;
    const v = S.state ? S.state.version : "";
    const foot = $("#nav-foot");
    foot.textContent = `Clipback ${v}`;
    foot.classList.toggle("has-update", upd.state === "available");
    if (upd.state === "available") foot.textContent += ` · есть ${upd.info.version}`;
    $("#upd-title").textContent =
      upd.state === "available" ? `Доступна версия ${upd.info.version}` : upd.state === "loading" ? `Обновление до ${upd.info.version}` : `Версия ${v}`;
    const notes = {
      idle: "Проверка ещё не выполнялась",
      checking: "Проверяю…",
      none: "Установлена последняя версия",
      available: upd.info && upd.info.portable ? "Программа заменит себя и перезапустится" : "Откроется установщик, программа перезапустится",
      loading: "Загрузка обновления…",
      error: upd.error,
    };
    $("#upd-note").textContent = notes[upd.state];
    $("#upd-check").disabled = upd.state === "checking" || upd.state === "loading";
    $("#upd-install").hidden = !(upd.state === "available" || upd.state === "loading");
    $("#upd-install").disabled = upd.state === "loading";
    if (upd.state !== "loading") $("#upd-progress").hidden = true;
  }
  async function checkUpdate() {
    upd.state = "checking";
    renderUpdate();
    try {
      upd.info = await api.call("check_update");
      upd.state = upd.info ? "available" : "none";
    } catch (e) {
      upd.state = "error";
      upd.error = String(e);
    }
    renderUpdate();
  }
  $("#upd-check").onclick = checkUpdate;
  $("#upd-install").onclick = async () => {
    upd.state = "loading";
    renderUpdate();
    $("#upd-progress").hidden = false;
    try {
      await api.call("install_update");
    } catch (e) {
      upd.state = "error";
      upd.error = String(e);
      renderUpdate();
    }
  };
  api.on("update-progress", (p) => {
    if (p.total) $("#upd-progress").firstElementChild.style.width = `${Math.round((p.done / p.total) * 100)}%`;
  });
  api.on("update-available", (info) => {
    upd.info = info;
    upd.state = "available";
    renderUpdate();
    if (MODE === "window") toast(`Доступна версия ${info.version}`, "Обновить можно в настройках → Программа", "info");
  });
  $("#nav-foot").addEventListener("click", () => { if (upd.state === "available") openSettings("app"); });
  $("#set-monitor").addEventListener("change", (e) => saveSettings({ monitor: +e.target.value }));
  $("#set-height").addEventListener("change", (e) => saveSettings({ maxHeight: e.target.value ? +e.target.value : null }));
  $$("#set-fps .seg-btn").forEach((b) => b.addEventListener("click", () => saveSettings({ fps: +b.dataset.v })));
  $$("#set-quality .seg-btn").forEach((b) => b.addEventListener("click", () => saveSettings({ quality: b.dataset.v })));
  $("#set-custom-bitrate").addEventListener("input", (e) => {
    $("#set-custom-bitrate-val").textContent = `${e.target.value} Мбит/с`;
    $("#set-bitrate").textContent = `≈ ${e.target.value} Мбит/с — около ${perMinute(+e.target.value)} за минуту видео`;
  });
  $("#set-custom-bitrate").addEventListener("change", (e) => saveSettings({ customBitrate: +e.target.value }));

  // звук
  $$("#set-audio-mode .seg-btn").forEach((b) => b.addEventListener("click", () => saveSettings({ audioMode: b.dataset.v })));
  $("#set-system-audio").addEventListener("change", (e) => saveSettings({ systemAudio: e.target.checked }));
  $("#set-mic").addEventListener("change", (e) => saveSettings({ mic: e.target.checked }));
  $("#set-mic-device").addEventListener("change", (e) => saveSettings({ micDevice: e.target.value || null }));
  $("#set-mic-gain").addEventListener("input", (e) => ($("#set-mic-gain-val").textContent = `×${(e.target.value / 100).toFixed(2).replace(/\.?0+$/, "")}`));
  $("#set-mic-gain").addEventListener("change", (e) => saveSettings({ micGain: e.target.value / 100 }));

  function refreshMics() {
    return api.call("list_mics").then((m) => { S.mics = m; fillSettings(); }).catch(() => {});
  }

  function currentMic() {
    const s = S.state && S.state.settings;
    const mics = S.mics || [];
    if (!s) return null;
    return mics.find((m) => (s.micDevice ? m.id === s.micDevice : m.isDefault)) || null;
  }

  function renderMicHint() {
    const s = S.state.settings;
    const m = currentMic();
    let hint = "Скажите что-нибудь — полоска должна двигаться";
    if (!s.mic) hint = "Микрофон выключен";
    else if (!S.state.recording) hint = "Включите запись буфера, чтобы видеть уровень";
    else if (m && m.muted) hint = "Микрофон выключен в Windows (Параметры → Звук → Ввод)";
    else if (m && m.volume < 0.5) hint = `Громкость микрофона в Windows всего ${Math.round(m.volume * 100)}% — если вас тихо слышно, поднимите её или усиление ниже`;
    $("#mic-hint").textContent = hint;
    $("#mic-hint").classList.toggle("warn-text", !!m && s.mic && (m.muted || m.volume < 0.5));
  }

  // индикатор уровня: опрашиваем, пока открыта вкладка «Звук»
  let meterTimer = null;
  function syncMeter() {
    const on = $("#settings").classList.contains("open") && $(".tab.active") && $(".tab.active").dataset.tab === "audio";
    if (on && !meterTimer) {
      meterTimer = setInterval(async () => {
        try {
          S.mics = await api.call("list_mics");
        } catch {
          return;
        }
        const m = currentMic();
        const s = S.state.settings;
        const peak = m && s.mic && S.state.recording ? Math.min(1, m.peak * s.micGain) : 0;
        const db = peak > 0 ? 20 * Math.log10(peak) : -90;
        $("#mic-meter").style.width = `${clamp(((db + 60) / 60) * 100, 0, 100)}%`;
        $("#mic-meter").classList.toggle("hot", db > -3);
        renderMicHint();
      }, 120);
    } else if (!on && meterTimer) {
      clearInterval(meterTimer);
      meterTimer = null;
      $("#mic-meter").style.width = "0%";
    }
  }

  // дорожки приложений (расширенный режим)
  function refreshApps() {
    return api.call("list_audio_apps").then((a) => { S.apps = a; renderAppTracks(); }).catch(() => {});
  }

  function renderAppTracks() {
    const s = S.state && S.state.settings;
    if (!s) return;
    const box = $("#app-list");
    box.innerHTML = "";
    if (!s.appTracks.length) {
      const e = document.createElement("div");
      e.className = "muted small-note";
      e.textContent = "Пока нет — выберите приложение ниже. Оно должно быть запущено и хоть раз воспроизвести звук.";
      box.append(e);
    }
    s.appTracks.forEach((a, i) => {
      const row = document.createElement("div");
      row.className = "app-row";
      row.innerHTML = `<label class="switch sm"><input type="checkbox"><span></span></label><input class="app-name" maxlength="40"><span class="mono muted app-exe"></span><button class="icon-btn sm danger" title="Убрать дорожку"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6L6 18"/></svg></button>`;
      const [cb, name] = row.querySelectorAll("input");
      cb.checked = a.enabled;
      name.value = a.name;
      row.querySelector(".app-exe").textContent = a.exe;
      const update = (patch) => saveSettings({ appTracks: s.appTracks.map((x, k) => (k === i ? { ...x, ...patch } : x)) });
      cb.addEventListener("change", () => update({ enabled: cb.checked }));
      name.addEventListener("change", () => update({ name: name.value.trim() || a.exe.replace(/\.exe$/i, "") }));
      name.addEventListener("keydown", (e) => { e.stopPropagation(); if (e.key === "Enter") name.blur(); });
      row.querySelector("button").onclick = () => saveSettings({ appTracks: s.appTracks.filter((_, k) => k !== i) });
      box.append(row);
    });
    const sel = $("#add-app-select");
    const used = new Set(s.appTracks.map((a) => a.exe.toLowerCase()));
    const apps = (S.apps || []).filter((a) => !used.has(a.exe.toLowerCase()));
    sel.innerHTML = apps.length ? apps.map(() => "<option></option>").join("") : `<option value="">Нет приложений со звуком</option>`;
    apps.forEach((a, i) => {
      sel.options[i].value = a.exe;
      sel.options[i].textContent = `${a.name} (${a.exe})`;
    });
    $("#add-app-btn").disabled = !apps.length || s.appTracks.length >= 8;
  }

  $("#add-app-btn").onclick = () => {
    const exe = $("#add-app-select").value;
    const app = (S.apps || []).find((a) => a.exe === exe);
    if (!app) return;
    saveSettings({ appTracks: [...S.state.settings.appTracks, { name: app.name, exe: app.exe, enabled: true }] });
  };
  $("#refresh-apps").onclick = refreshApps;

  $("#set-outdir-open").onclick = () => call("open_clips_folder", {}, "Не удалось открыть папку");
  $("#set-outdir-pick").onclick = async () => {
    const p = await call("pick_folder", {}, "Не удалось выбрать папку");
    if (p) await saveSettings({ outDir: p });
  };
  $("#btn-quit").onclick = async () => {
    if (await confirmBox("Выйти из Clipback?", "Запись буфера остановится. Уже сохранённые клипы останутся.", "Выйти")) call("quit_app");
  };

  // назначение горячих клавиш
  let capturing = null;
  function keyName(e) {
    const c = e.code;
    if (/^Key[A-Z]$/.test(c)) return c.slice(3);
    if (/^Digit\d$/.test(c)) return c.slice(5);
    if (/^F\d{1,2}$/.test(c)) return c;
    if (["ControlLeft", "ControlRight", "AltLeft", "AltRight", "ShiftLeft", "ShiftRight", "MetaLeft", "MetaRight"].includes(c)) return null;
    return c; // Space, Backquote, Numpad1, Insert, PageUp…
  }
  function onCaptureKey(e) {
    e.preventDefault();
    // нажатие уходит только сюда: Esc не должен заодно закрыть настройки или оверлей
    e.stopImmediatePropagation();
    if (e.code === "Escape") return stopKeyCapture();
    const key = keyName(e);
    if (!key) return;
    const mods = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Super"].filter(Boolean);
    if (!mods.length && !/^F\d{1,2}$/.test(key)) {
      capturing.textContent = "Нужен Ctrl, Alt или Shift";
      return;
    }
    const combo = [...mods, key].join("+");
    const field = capturing.dataset.key;
    const s = S.state.settings;
    const clash = ["hotkeyOverlay", "hotkeySave", "hotkeyScreenshot", "hotkeyRecord"].find((k) => k !== field && s[k].toLowerCase() === combo.toLowerCase());
    stopKeyCapture();
    if (clash) return toast("Сочетание уже занято", `${combo} используется для другого действия`, "error");
    saveSettings({ [field]: combo });
  }
  function stopKeyCapture() {
    if (!capturing) return;
    capturing.classList.remove("listening");
    capturing = null;
    removeEventListener("keydown", onCaptureKey, true);
    fillSettings();
  }
  $$(".keybind").forEach((k) =>
    k.addEventListener("click", () => {
      stopKeyCapture();
      capturing = k;
      k.classList.add("listening");
      k.textContent = "Нажмите сочетание…";
      addEventListener("keydown", onCaptureKey, true);
    })
  );

  // ---------- клавиатура ----------
  addEventListener("keydown", (e) => {
    if (MODE === "browser" && e.altKey && e.code === "KeyZ") {
      e.preventDefault();
      const open = document.body.classList.toggle("closed");
      document.documentElement.classList.toggle("overlay-open", !open);
      return;
    }
    if (document.body.classList.contains("closed")) return;
    if (e.target.closest && e.target.closest("input, select, [contenteditable=true]")) return;
    if ($("#confirm").classList.contains("open")) {
      if (e.key === "Escape") closeConfirm(false);
      return;
    }
    if (e.key === "Escape") {
      if ($("#settings").classList.contains("open")) return closeSettings();
      if (isFullView()) return setFullView(false);
      if (MODE === "overlay" || MODE === "browser") return call("hide_overlay_cmd");
      return;
    }
    if ($("#settings").classList.contains("open")) return;
    const fps = (S.clip && S.clip.fps) || 60;
    const list = visible();
    const idx = S.clip ? list.findIndex((c) => c.path === S.clip.path) : -1;
    switch (e.code) {
      case "Space": e.preventDefault(); setPlaying(!S.playing); break;
      case "KeyI": setIn(); break;
      case "KeyO": setOut(); break;
      case "ArrowLeft": e.preventDefault(); seek(S.t - (e.shiftKey ? 1 : 5), true); break;
      case "ArrowRight": e.preventDefault(); seek(S.t + (e.shiftKey ? 1 : 5), true); break;
      case "Comma": setPlaying(false); seek(S.t - 1 / fps, true); break;
      case "Period": setPlaying(false); seek(S.t + 1 / fps, true); break;
      case "Home": seek(S.inT); break;
      case "End": seek(S.outT); break;
      case "ArrowUp": e.preventDefault(); if (idx > 0) selectClip(list[idx - 1]); break;
      case "ArrowDown": e.preventDefault(); if (idx >= 0 && idx < list.length - 1) selectClip(list[idx + 1]); break;
      case "KeyM": video.muted = !video.muted; renderVolume(); break;
      case "KeyF": setFullView(!isFullView()); break;
      case "KeyE": editShot(); break;
      case "Delete": deleteClip(); break;
      case "KeyS":
        if (e.ctrlKey) { e.preventDefault(); exportClip(); }
        else saveFrame();
        break;
    }
  });

  // ---------- демо-фон в браузере ----------
  const bg = $("#game-bg");
  const bgCtx = bg.getContext("2d");
  const bgClip = { hue: 28, seed: 42, markers: [] };
  function drawGameBg(now) {
    if (bg.width !== Math.round(innerWidth / 2)) { bg.width = Math.round(innerWidth / 2); bg.height = Math.round(innerHeight / 2); }
    if (document.body.classList.contains("closed") || Math.floor(now / 50) % 2 === 0) api.drawDemo(bgCtx, bg.width, bg.height, now / 1000, bgClip);
  }

  // ---------- события от ядра ----------
  api.on("state", (st) => { S.state = st; renderState(); });
  api.on("clips-changed", () => loadClips());
  api.on("clip-saved", (path) => {
    if (MODE === "browser") toast("Повтор сохранён", "", "replay");
    S.pendingSelect = path;
    loadClips();
  });
  api.on("overlay-will-show", () => document.documentElement.classList.remove("overlay-open"));
  api.on("overlay-shown", () => {
    // два кадра: окно успевает показаться прозрачным, потом интерфейс плавно проявляется
    requestAnimationFrame(() => requestAnimationFrame(() => document.documentElement.classList.add("overlay-open")));
    loadClips();
  });
  api.on("overlay-hide", () => {
    document.documentElement.classList.remove("overlay-open");
    setPlaying(false);
  });
  api.on("overlay-hidden", () => setPlaying(false));
  // щелчок мимо окна Clipback закрывает оверлей
  $("#backdrop").addEventListener("pointerdown", () => call("hide_overlay_cmd"));

  // ---------- старт ----------
  (async () => {
    renderVolume();
    resizeCanvas();
    try {
      S.state = await api.call("get_state");
      renderState();
    } catch (e) {
      toast("Нет связи с ядром", String(e), "error");
    }
    await loadClips().catch(() => {});
    requestAnimationFrame(frame);
  })();
}
