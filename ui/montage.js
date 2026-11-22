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
    // Оба края задаются курсором в исходнике. Один жест может пересекать старые вырезки.
    removeRange(p, a, b, min) {
      const lo = Math.min(a, b), hi = Math.max(a, b);
      if (hi - lo + EPS < min) return p;
      const kept = [];
      let removed = 0;
      for (const pc of p) {
        const s = Math.max(lo, pc.s), e = Math.min(hi, pc.e);
        if (e <= s) {
          kept.push(pc);
          continue;
        }
        removed += e - s;
        if (s > pc.s) kept.push({ s: pc.s, e: s });
        if (e < pc.e) kept.push({ s: e, e: pc.e });
      }
      return removed > EPS && removed + EPS >= min ? kept : p;
    },
    // Один разрез без вырезки не прерывает исходный фрагмент при экспорте.
    exportRanges(p) {
      const ranges = [];
      for (const { s, e } of p) {
        const last = ranges[ranges.length - 1];
        if (last && last.e === s) last.e = e;
        else ranges.push({ s, e });
      }
      return ranges;
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

  /// История правок. Плеер хранит список кусков или границы обычного отрезка.
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
    const M = { hist: History(), anchor: null, sel: -1, selecting: false };
    let keys = loadKeys();
    const active = () => Array.isArray(M.hist.cur);
    const pieces = () => active() ? M.hist.cur : null;
    const isVideo = () => !!(S.clip && S.clip.kind === "video" && !S.clip.broken);
    const minLen = () => 1 / ((S.clip && S.clip.fps) || 60);

    // слой поверх таймлайна: убранные промежутки, стыки, выделенный кусок
    const layer = document.createElement("div");
    layer.className = "mt-layer";
    tl.insertBefore(layer, $("#playhead"));
    const draft = document.createElement("div");
    draft.className = "mt-draft";
    draft.hidden = true;
    tl.insertBefore(draft, $("#playhead"));

    const tools = document.createElement("div");
    tools.className = "mt-tools";
    tools.innerHTML = `
      <button type="button" class="chip mt-range">Убрать фрагмент…</button>
      <button type="button" class="chip mt-cancel" hidden>Отмена</button>
      <button type="button" class="chip mt-undo" title="Ctrl+Z" disabled>Отменить правку</button>
      <button type="button" class="chip mt-redo" title="Ctrl+Y" hidden>Повторить</button>
      <details class="mt-keys"><summary>Клавиши</summary><div class="mt-hint"></div></details>
      <span class="mt-prompt"></span>`;
    tl.after(tools);
    const rangeBtn = tools.querySelector(".mt-range");
    const cancelBtn = tools.querySelector(".mt-cancel");
    const undoBtn = tools.querySelector(".mt-undo");
    const redoBtn = tools.querySelector(".mt-redo");
    const prompt = tools.querySelector(".mt-prompt");
    const hint = tools.querySelector(".mt-hint");
    const preview = document.createElement("section");
    preview.className = "mt-preview field";
    preview.hidden = true;
    preview.innerHTML = `<b>В один файл</b><div class="mt-count" role="status"></div>
      <div class="mt-source">Время в исходном клипе</div><ol aria-label="Время в исходном клипе"></ol>`;
    $("#export .panel-head").after(preview);

    const start = () => pieces() || [{ s: S.inT, e: S.outT }];
    function updateRange() {
      if (!isVideo() || S.busy) return;
      if (S.playing) h.setPlaying(false);
      if (M.anchor === null) {
        M.anchor = S.t;
        M.selecting = false;
        M.sel = -1;
        h.updateAll();
      } else {
        const p = start();
        const next = Cuts.removeRange(p, M.anchor, S.t, minLen());
        if (next !== p) apply(next);
      }
    }
    function cancelRange() {
      M.anchor = null;
      h.updateAll();
    }
    rangeBtn.onclick = updateRange;
    cancelBtn.onclick = cancelRange;
    undoBtn.onclick = undo;
    redoBtn.onclick = redo;

    function apply(next) {
      if (!active()) M.hist.cur = { inT: S.inT, outT: S.outT };
      if (!M.hist.apply(next)) return;
      M.anchor = null;
      M.sel = -1;
      changed();
    }
    function changed() {
      if (active()) {
        S.inT = 0;
        S.outT = S.clip.duration;
      } else if (M.hist.cur) {
        S.inT = M.hist.cur.inT;
        S.outT = M.hist.cur.outT;
      }
      if (!active()) M.selecting = false;
      h.updateAll();
    }

    function edit(id) {
      const p = start();
      const action = { cut: Cuts.cut, left: Cuts.removeLeft, right: Cuts.removeRight }[id];
      const next = action(p, S.t, minLen());
      if (next !== p) apply(next);
    }

    function undo() {
      if (!isVideo() || S.busy || M.anchor !== null) return;
      if (M.hist.back()) (M.sel = -1), changed();
    }
    function redo() {
      if (!isVideo() || S.busy || M.anchor !== null) return;
      if (M.hist.forward()) (M.sel = -1), changed();
    }

    function key(e) {
      if (!isVideo() || S.busy || capturing) return false;
      if ($("#settings").classList.contains("open")) return false;
      const ctrl = e.ctrlKey || e.metaKey;
      if (M.anchor !== null) {
        if (e.key === "Escape" || (ctrl && e.code === "KeyZ" && !e.shiftKey)) {
          e.preventDefault();
          cancelRange();
          return true;
        }
        // Пока выбирается край, Delete не должен уходить в корзину библиотеки.
        if (["Delete", "Backspace"].includes(e.code) || ACTIONS.some((a) => keys[a.id].includes(e.code))) {
          e.preventDefault();
          return true;
        }
      }
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
        if (!M.selecting || !active() || e.button !== 0 || e.target.closest("#tracks, .ph-knob")) return;
        e.preventDefault();
        e.stopImmediatePropagation();
        const r = tl.getBoundingClientRect();
        M.sel = Cuts.find(pieces(), h.xToT(e.clientX - r.left));
        render();
      },
      true
    );
    // «Весь клип» убирает и монтаж (вернуть — Ctrl+Z)
    $("#btn-reset").addEventListener("click", () => {
      if (active()) {
        apply({ inT: 0, outT: S.clip.duration });
      } else if (M.anchor !== null) cancelRange();
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

    let shownPieces, shownSelection;
    function render() {
      const on = active() && isVideo();
      tl.classList.toggle("mt-on", on);
      tl.classList.toggle("mt-selecting", on && M.selecting);
      document.body.classList.toggle("montage-on", on);
      tools.hidden = !isVideo();
      preview.hidden = !on;
      renderRange();
      if (on !== M.wasOn) {
        M.wasOn = on;
        $("#export .panel-head span").textContent = on ? "Монтаж" : "Отрезок";
        const btn = $("#btn-export");
        const label = [...btn.childNodes].reverse().find((n) => n.nodeType === 3 && n.textContent.trim());
        if (label) label.textContent = on ? " Сохранить монтаж " : " Сохранить отрезок ";
        h.renderQuality();
        h.updateExport();
      }
      const p = on ? pieces() : null;
      if (p !== shownPieces && on) renderPreview(p);
      if (p !== shownPieces || M.sel !== shownSelection) {
        if (on) renderPieces(p);
        else layer.replaceChildren();
        shownPieces = p;
        shownSelection = M.sel;
      }
      if (on) {
        $("#sel-len").textContent = h.fmt(Cuts.total(p), 1);
        $("#sel-range").textContent = p.length ? `${h.fmt(Cuts.srcToOut(p, S.t), 1)} в монтаже` : "всё убрано";
      }
    }

    function bounds(s, e) {
      return `${h.fmt(s, 2)} – ${h.fmt(e, 2)}`;
    }
    function renderPreview(p) {
      const ranges = Cuts.exportRanges(p);
      const total = Cuts.total(p);
      let summary = `${ranges.length} ${plural(ranges.length)} · ${h.fmt(total, 2)}`;
      if (!ranges.length) summary += " · Всё убрано. Отмените правку.";
      else if (total < 0.1) summary += " · Для сохранения нужно хотя бы 0,1 с.";
      preview.querySelector(".mt-count").textContent = summary;
      preview.querySelector(".mt-source").hidden = !ranges.length;
      preview.querySelector("ol").replaceChildren(...ranges.map(({ s, e }) => {
        const li = document.createElement("li");
        li.textContent = bounds(s, e);
        return li;
      }));
    }
    function position(el, s, e) {
      el.style.left = `${100 * s / S.clip.duration}%`;
      el.style.width = `${100 * (e - s) / S.clip.duration}%`;
    }
    function renderPieces(p) {
      const els = [];
      const gap = (a, b) => {
        if (b - a < EPS) return;
        const g = document.createElement("div");
        g.className = "mt-gap";
        position(g, a, b);
        els.push(g);
      };
      let prev = 0;
      p.forEach((pc, i) => {
        gap(prev, pc.s);
        const el = document.createElement("div");
        el.className = "mt-piece" + (i === M.sel ? " picked" : "");
        position(el, pc.s, pc.e);
        els.push(el);
        // стык двух кусков без промежутка — видимый разрез
        if (i > 0 && Math.abs(p[i - 1].e - pc.s) < EPS) {
          const c = document.createElement("div");
          c.className = "mt-cut";
          c.style.left = el.style.left;
          els.push(c);
        }
        prev = pc.e;
      });
      gap(prev, S.clip.duration);
      layer.replaceChildren(...els);
    }

    function renderRange() {
      const marking = isVideo() && M.anchor !== null;
      draft.hidden = cancelBtn.hidden = !marking;
      rangeBtn.textContent = marking ? "Убрать фрагмент" : "Убрать фрагмент…";
      undoBtn.disabled = marking || S.busy || !M.hist.undo.length;
      redoBtn.hidden = marking || !M.hist.redo.length;
      redoBtn.disabled = S.busy;
      rangeBtn.disabled = S.busy || (active() && !pieces().length);
      if (marking) {
        const s = Math.min(M.anchor, S.t), e = Math.max(M.anchor, S.t);
        position(draft, s, e);
        const p = start();
        const canRemove = Cuts.removeRange(p, s, e, minLen()) !== p;
        rangeBtn.disabled = S.busy || !canRemove;
        prompt.textContent = canRemove ? `Вырезать ${bounds(s, e)}` : "Укажите другой край вырезки на шкале.";
      } else {
        prompt.textContent = M.selecting ? "Щёлкните по куску на шкале." : "Поставьте курсор на начало ненужного фрагмента.";
        if (active() && !pieces().length) prompt.textContent = "Отмените правку, чтобы вернуть фрагменты.";
      }
    }

    function renderHint() {
      const k = (id) => keys[id].map(keyLabel).join(" / ");
      const items = [
        [k("cut"), "разрезать"],
        [k("left"), "убрать слева"],
        [k("right"), "убрать справа"],
        [k("select"), "выбор куска"],
        [k("remove"), "убрать кусок"],
        ["Ctrl+Z", "отменить"],
      ];
      hint.replaceChildren(...items.map(([kk, t]) => {
        const item = document.createElement("span");
        const cap = document.createElement("kbd");
        cap.textContent = kk;
        item.append(cap, " " + t);
        return item;
      }));
    }

    function reset() {
      M.hist = History();
      M.anchor = null;
      M.sel = -1;
      M.selecting = false;
    }

    // ---------- сохранение ----------
    async function save() {
      const c = S.clip;
      if (!isVideo() || S.busy || !active() || M.anchor !== null) return;
      const p = pieces();
      if (!p.length || Cuts.total(p) < 0.1) return h.toast("Нечего сохранять", "Все куски убраны — Ctrl+Z вернёт", "error");
      S.busy = true;
      h.updateExport();
      h.work.start(h.quality().startsWith("discord:") ? "Сжимаю монтаж под размер…" : "Сохраняю монтаж…");
      try {
        const r = await h.api.call("montage_clip", {
          path: c.path,
          pieces: Cuts.toPairs(p),
          audioKeep: h.audioKeep(c),
          audioGains: h.audioGains(c),
          mixdown: S.mixdown || S.exportQuality === "discord",
          quality: h.quality(),
        });
        const sub = `${h.fmt(r.duration, 1)} · ${r.pieces} ${plural(r.pieces)} · ${h.mb(r.clip.size)}`;
        if (!h.work.done(`Монтаж сохранён · ${sub}`, r.clip.path)) h.toast("Монтаж сохранён", sub, "scissors");
        S.busy = false;
        await h.loadClips(r.clip.path);
      } catch (e) {
        h.work.fail();
        if (/отменено/.test(String(e))) h.toast("Сохранение отменено", "", "info");
        else h.toast("Не удалось сохранить монтаж", String(e), "error");
      } finally {
        S.busy = false;
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
      renderHint();
      render();
    }
    const resetKeys = $("#montage-keys-reset");
    if (resetKeys)
      resetKeys.onclick = () => {
        keys = Object.fromEntries(ACTIONS.map((a) => [a.id, a.keys.slice()]));
        store.set("clipback.montageKeys", JSON.stringify(keys));
        fillKeys();
        renderHint();
        render();
      };
    fillKeys();
    renderHint();

    return {
      active: () => active() && isVideo(),
      total: () => (active() ? Cuts.total(pieces()) : 0),
      key,
      tick,
      beforePlay,
      render,
      // «Оригинал» у монтажа — не копия без потерь: видео перекодируется с битрейтом исходника
      qualityHint() {
        if (M.anchor !== null) $("#btn-export").disabled = true;
        if (active() && isVideo() && S.exportQuality === "original") $("#quality-hint").textContent = "Как в оригинале, видео пересобирается видеокартой";
      },
      reset,
      save,
    };
  }

  if (typeof module !== "undefined" && module.exports) module.exports = { Cuts, History };
  else window.Montage = { Cuts, History, create };
})();
