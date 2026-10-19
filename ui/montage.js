// Монтаж в плеере: клип режется на куски, лишние убираются, оставшиеся сохраняются одним файлом.
// Куски — оставленные части исходника [{ s, e }] в секундах, по порядку, без наложений.
// Убранное — промежутки между ними. Предпросмотр играет куски подряд, промежутки пропускает.

(function () {
  const EPS = 1e-6;

  // ---------- список кусков (чистые функции, проверяются в tests/montage.test.js) ----------
  const Cuts = {
    total(p) {
      return p.reduce((a, x) => a + (x.e - x.s), 0);
    },
    /// Кусок, в котором стоит t (s ≤ t < e), или -1.
    find(p, t) {
      return p.findIndex((x) => t >= x.s - EPS && t < x.e - EPS);
    },
    /// V — разрезать кусок в t. Слишком близко к краю (меньше min) — ничего.
    cut(p, t, min) {
      const i = Cuts.find(p, t);
      if (i < 0 || t - p[i].s < min || p[i].e - t < min) return p;
      return [...p.slice(0, i), { s: p[i].s, e: t }, { s: t, e: p[i].e }, ...p.slice(i + 1)];
    },
    /// Q — убрать всё левее t в этом куске.
    removeLeft(p, t, min) {
      const i = Cuts.find(p, t);
      if (i < 0 || t - p[i].s < min) return p;
      if (p[i].e - t < min) return Cuts.remove(p, i);
      return [...p.slice(0, i), { s: t, e: p[i].e }, ...p.slice(i + 1)];
    },
    /// E — убрать всё правее t в этом куске.
    removeRight(p, t, min) {
      const i = Cuts.find(p, t);
      if (i < 0 || p[i].e - t < min) return p;
      if (t - p[i].s < min) return Cuts.remove(p, i);
      return [...p.slice(0, i), { s: p[i].s, e: t }, ...p.slice(i + 1)];
    },
    /// Delete — убрать кусок целиком (остальные смыкаются).
    remove(p, i) {
      return i >= 0 && i < p.length ? [...p.slice(0, i), ...p.slice(i + 1)] : p;
    },
    /// Время исходника → время в готовом монтаже. Из убранного — начало следующего куска.
    srcToOut(p, t) {
      let acc = 0;
      for (const x of p) {
        if (t < x.s) return acc;
        if (t < x.e) return acc + (t - x.s);
        acc += x.e - x.s;
      }
      return acc;
    },
    /// Время в монтаже → время исходника.
    outToSrc(p, o) {
      let acc = 0;
      for (const x of p) {
        const len = x.e - x.s;
        if (o < acc + len) return x.s + Math.max(0, o - acc);
        acc += len;
      }
      return p.length ? p[p.length - 1].e : 0;
    },
    /// Откуда играть: t, если оно в куске, иначе начало следующего куска; null — дальше ничего.
    nextPlayable(p, t) {
      for (const x of p) if (t < x.e - EPS) return Math.max(t, x.s);
      return null;
    },
    /// Куски, которые отправляются на сохранение: [[s, e], …].
    toPairs(p) {
      return p.map((x) => [x.s, x.e]);
    },
  };

  /// История правок: снимки списка (null — монтажа нет). Ctrl+Z / Ctrl+Y.
  function History() {
    return {
      cur: null,
      undo: [],
      redo: [],
      apply(next) {
        if (next === this.cur) return false;
        this.undo.push(this.cur);
        this.redo = [];
        this.cur = next;
        return true;
      },
      back() {
        if (!this.undo.length) return false;
        this.redo.push(this.cur);
        this.cur = this.undo.pop();
        return true;
      },
      forward() {
        if (!this.redo.length) return false;
        this.undo.push(this.cur);
        this.cur = this.redo.pop();
        return true;
      },
    };
  }

  // ---------- клавиши ----------
  const ACTIONS = [
    { id: "cut", name: "Разрезать", sub: "в позиции курсора", keys: ["KeyV"] },
    { id: "left", name: "Убрать слева", sub: "от начала куска до курсора", keys: ["KeyQ"] },
    { id: "right", name: "Убрать справа", sub: "от курсора до конца куска", keys: ["KeyE"] },
    { id: "select", name: "Выбор кусков", sub: "щелчок по куску выделяет его", keys: ["KeyA"] },
    { id: "remove", name: "Убрать выделенный кусок", sub: "остальные смыкаются", keys: ["Delete", "Backspace"] },
  ];
  // клавиши плеера, которые занимать нельзя
  const RESERVED = ["Space", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Comma", "Period", "Escape", "Home", "End", "KeyI", "KeyO", "KeyM", "KeyF", "KeyS", "F2", "Tab", "Enter"];
  const keyLabel = (code) =>
    /^Key[A-Z]$/.test(code) ? code.slice(3) : /^Digit\d$/.test(code) ? code.slice(5) : { Delete: "Delete", Backspace: "Backspace", Minus: "-", Equal: "=", BracketLeft: "[", BracketRight: "]", Semicolon: ";", Quote: "'", Backslash: "\\", Slash: "/", Backquote: "`" }[code] || code;

  const store = {
    get(k) { try { return localStorage.getItem(k); } catch { return null; } },
    set(k, v) { try { localStorage.setItem(k, v); } catch {} },
  };
  function loadKeys() {
    const map = Object.fromEntries(ACTIONS.map((a) => [a.id, a.keys.slice()]));
    try {
      const saved = JSON.parse(store.get("clipback.montageKeys") || "{}");
      for (const a of ACTIONS) if (Array.isArray(saved[a.id]) && saved[a.id].length) map[a.id] = saved[a.id].filter((c) => typeof c === "string");
    } catch {}
    return map;
  }

  const plural = (n) => (n % 10 === 1 && n % 100 !== 11 ? "кусок" : n % 10 >= 2 && n % 10 <= 4 && (n % 100 < 12 || n % 100 > 14) ? "куска" : "кусков");

  // ---------- плеер ----------
  /// h — то, что даёт плеер (app.js): состояние S и его функции.
  function create(h) {
    const { S, $, tl } = h;
    const M = { hist: History(), base: null, sel: -1, selecting: false, saving: false };
    let keys = loadKeys();
    const pieces = () => M.hist.cur;
    const active = () => M.hist.cur !== null;
    const isVideo = () => !!(S.clip && S.clip.kind === "video" && !S.clip.broken);
    const minLen = () => 1 / ((S.clip && S.clip.fps) || 60);

    // слой поверх таймлайна: убранные промежутки, стыки, выделенный кусок
    const layer = document.createElement("div");
    layer.className = "mt-layer";
    tl.insertBefore(layer, $("#playhead"));
    const hint = document.createElement("div");
    hint.className = "mt-hint";
    tl.after(hint);
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "btn btn-ghost wide mt-cancel";
    cancelBtn.textContent = "Отмена";
    cancelBtn.hidden = true;
    $("#export-progress").after(cancelBtn);
    cancelBtn.onclick = () => h.api.call("cancel_montage").catch(() => {});

    function apply(next) {
      if (!M.hist.apply(next)) return;
      M.sel = -1;
      changed();
    }
    // первая правка начинается с выделенного отрезка (или всего клипа)
    function start() {
      if (active()) return pieces();
      M.base = { inT: S.inT, outT: S.outT };
      return [{ s: S.inT, e: S.outT }];
    }
    function changed() {
      if (active()) {
        S.inT = 0;
        S.outT = S.clip.duration;
      } else if (M.base) {
        S.inT = M.base.inT;
        S.outT = M.base.outT;
      }
      if (!active()) M.selecting = false;
      h.updateAll();
    }

    function edit(id) {
      const t = S.t;
      if (id === "cut") apply(Cuts.cut(start(), t, minLen()));
      else if (id === "left") apply(Cuts.removeLeft(start(), t, minLen()));
      else if (id === "right") apply(Cuts.removeRight(start(), t, minLen()));
    }

    function undo() {
      if (M.hist.back()) (M.sel = -1), changed();
    }
    function redo() {
      if (M.hist.forward()) (M.sel = -1), changed();
    }

    function key(e) {
      if (!isVideo() || S.busy || capturing) return false;
      if ($("#settings").classList.contains("open")) return false;
      const ctrl = e.ctrlKey || e.metaKey;
      if (ctrl && !e.altKey) {
        if (e.code === "KeyZ" && !e.shiftKey && M.hist.undo.length) return e.preventDefault(), undo(), true;
        if ((e.code === "KeyY" || (e.code === "KeyZ" && e.shiftKey)) && M.hist.redo.length) return e.preventDefault(), redo(), true;
        return false;
      }
      if (ctrl || e.altKey) return false;
      if (e.key === "Escape" && (M.selecting || M.sel >= 0)) {
        M.selecting = false;
        M.sel = -1;
        render();
        return true;
      }
      const act = ACTIONS.find((a) => keys[a.id].includes(e.code));
      if (!act) return false;
      e.preventDefault();
      if (act.id === "select") {
        M.selecting = !M.selecting;
        if (!M.selecting) M.sel = -1;
        render();
      } else if (act.id === "remove") {
        if (M.sel >= 0 && active()) apply(Cuts.remove(pieces(), M.sel));
        else if (active()) h.toast("Сначала выберите кусок", `${keyLabel(keys.select[0])} и щелчок по куску на таймлайне`);
        else return e.code === "Backspace"; // Delete без монтажа — как раньше, клип в корзину
      } else edit(act.id);
      return true;
    }

    // в режиме выбора щелчок по таймлайну выделяет кусок, а не перематывает
    tl.addEventListener(
      "pointerdown",
      (e) => {
        if (!M.selecting || !active() || e.button !== 0 || e.target.closest(".ph-knob")) return;
        e.preventDefault();
        e.stopPropagation();
        const r = tl.getBoundingClientRect();
        M.sel = Cuts.find(pieces(), h.xToT(e.clientX - r.left));
        render();
      },
      true
    );
    // «Весь клип» убирает и монтаж (вернуть — Ctrl+Z)
    $("#btn-reset").addEventListener("click", () => {
      if (active()) {
        M.base = null;
        apply(null);
      }
    });

    /// Каждый кадр, пока играет: убранное пропускается, после последнего куска — пауза.
    function tick() {
      const n = Cuts.nextPlayable(pieces(), S.t);
      if (n === null) {
        const p = pieces();
        h.setPlaying(false);
        h.seek(p.length ? p[p.length - 1].e : S.t);
      } else if (n > S.t + 1e-3) h.seek(n);
    }
    /// Перед запуском: из убранного — к следующему куску, с конца — к первому.
    function beforePlay() {
      const p = pieces();
      if (!p.length) return false;
      const n = Cuts.nextPlayable(p, S.t);
      const to = n === null || (S.t >= p[p.length - 1].e - 0.05) ? p[0].s : n;
      if (Math.abs(to - S.t) > 1e-3) h.seek(to);
      return true;
    }

    function render() {
      const on = active() && isVideo();
      tl.classList.toggle("mt-on", on);
      tl.classList.toggle("mt-selecting", on && M.selecting);
      document.body.classList.toggle("montage-on", on);
      renderHint();
      const head = $("#export .panel-head span");
      if (head) head.textContent = on ? "Монтаж" : "Отрезок";
      const btn = $("#btn-export");
      const label = [...btn.childNodes].reverse().find((n) => n.nodeType === 3 && n.textContent.trim());
      if (label) label.textContent = on ? " Сохранить монтаж " : " Сохранить отрезок ";
      if (!on) {
        layer.replaceChildren();
        return;
      }
      const p = pieces();
      const d = S.clip.duration;
      const x = (t) => h.tToX(t);
      const els = [];
      const gap = (a, b) => {
        if (b - a < EPS) return;
        const g = document.createElement("div");
        g.className = "mt-gap";
        g.style.left = `${x(a)}px`;
        g.style.width = `${Math.max(1, x(b) - x(a))}px`;
        els.push(g);
      };
      let prev = 0;
      p.forEach((pc, i) => {
        gap(prev, pc.s);
        const el = document.createElement("div");
        el.className = "mt-piece" + (i === M.sel ? " sel" : "");
        el.style.left = `${x(pc.s)}px`;
        el.style.width = `${Math.max(2, x(pc.e) - x(pc.s))}px`;
        els.push(el);
        // стык двух кусков без промежутка — видимый разрез
        if (i > 0 && Math.abs(p[i - 1].e - pc.s) < EPS) {
          const c = document.createElement("div");
          c.className = "mt-cut";
          c.style.left = `${x(pc.s)}px`;
          els.push(c);
        }
        prev = pc.e;
      });
      gap(prev, d);
      layer.replaceChildren(...els);
      const total = Cuts.total(p);
      $("#sel-len").textContent = h.fmt(total, 1);
      $("#sel-range").textContent = p.length ? `${p.length} ${plural(p.length)} · ${h.fmt(Cuts.srcToOut(p, S.t), 1)} в монтаже` : "всё убрано";
    }

    function renderHint() {
      if (!isVideo()) return void (hint.hidden = true);
      hint.hidden = false;
      const k = (id) => keys[id].map(keyLabel).join(" / ");
      const items = [
        [k("cut"), "разрезать"],
        [k("left"), "убрать слева"],
        [k("right"), "убрать справа"],
        [k("select"), M.selecting ? "выбор: щёлкните кусок" : "выбор"],
        [k("remove"), "убрать кусок"],
        ["Ctrl+Z", "отменить"],
      ];
      hint.classList.toggle("selecting", M.selecting && active());
      hint.innerHTML = items.map(([kk, t]) => `<span><kbd>${kk}</kbd> ${t}</span>`).join("");
    }

    function reset() {
      M.hist = History();
      M.base = null;
      M.sel = -1;
      M.selecting = false;
    }

    // ---------- сохранение ----------
    async function save() {
      const c = S.clip;
      if (!c || S.busy || !active()) return;
      const p = pieces();
      if (!p.length || Cuts.total(p) < 0.1) return h.toast("Нечего сохранять", "Все куски убраны — Ctrl+Z вернёт", "error");
      S.busy = true;
      M.saving = true;
      cancelBtn.hidden = false;
      cancelBtn.disabled = false;
      h.updateExport();
      h.showProgress(0);
      try {
        const r = await h.api.call("montage_clip", {
          path: c.path,
          pieces: Cuts.toPairs(p),
          audioKeep: h.audioKeep(c),
          audioGains: h.audioGains(c),
          quality: h.quality(),
        });
        h.toast("Монтаж сохранён", `${h.fmt(r.duration, 1)} · ${r.pieces} ${plural(r.pieces)} · ${h.mb(r.clip.size)}`, "scissors");
        S.busy = false;
        await h.loadClips(r.clip.path);
      } catch (e) {
        if (/отменено/.test(String(e))) h.toast("Сохранение отменено", "", "info");
        else h.toast("Не удалось сохранить монтаж", String(e), "error");
      } finally {
        S.busy = false;
        M.saving = false;
        cancelBtn.hidden = true;
        h.showProgress(null);
        h.updateExport();
      }
    }

    // ---------- назначение клавиш (Настройки → Горячие клавиши) ----------
    let capturing = null;
    const box = $("#montage-keys");
    function fillKeys() {
      if (!box) return;
      box.innerHTML = ACTIONS.map(
        (a) => `<div class="opt"><div><b>${a.name}</b><small>${a.sub}</small></div><button class="mt-key${capturing === a.id ? " listening" : ""}" data-act="${a.id}">${capturing === a.id ? "Нажмите клавишу…" : keys[a.id].map(keyLabel).join(" / ")}</button></div>`
      ).join("");
      box.querySelectorAll(".mt-key").forEach((b) => (b.onclick = () => {
        capturing = b.dataset.act;
        addEventListener("keydown", onCapture, true);
        fillKeys();
      }));
    }
    function stopCapture() {
      capturing = null;
      removeEventListener("keydown", onCapture, true);
      fillKeys();
    }
    function onCapture(e) {
      e.preventDefault();
      e.stopImmediatePropagation();
      if (e.code === "Escape") return stopCapture();
      if (["ControlLeft", "ControlRight", "AltLeft", "AltRight", "ShiftLeft", "ShiftRight", "MetaLeft", "MetaRight"].includes(e.code)) return;
      const id = capturing;
      if (RESERVED.includes(e.code)) {
        stopCapture();
        return h.toast("Клавиша занята плеером", `${keyLabel(e.code)} уже делает другое`, "error");
      }
      const other = ACTIONS.find((a) => a.id !== id && keys[a.id].includes(e.code));
      if (other) {
        stopCapture();
        return h.toast("Клавиша уже занята", `${keyLabel(e.code)} — «${other.name}»`, "error");
      }
      keys[id] = [e.code];
      store.set("clipback.montageKeys", JSON.stringify(keys));
      stopCapture();
      render();
    }
    const resetKeys = $("#montage-keys-reset");
    if (resetKeys)
      resetKeys.onclick = () => {
        keys = Object.fromEntries(ACTIONS.map((a) => [a.id, a.keys.slice()]));
        store.set("clipback.montageKeys", JSON.stringify(keys));
        fillKeys();
        render();
      };
    fillKeys();

    return {
      active: () => active() && isVideo(),
      total: () => (active() ? Cuts.total(pieces()) : 0),
      key,
      tick,
      beforePlay,
      render,
      reset,
      save,
    };
  }

  if (typeof module !== "undefined" && module.exports) module.exports = { Cuts, History };
  else window.Montage = { Cuts, History, create };
})();
