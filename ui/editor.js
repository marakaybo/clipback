// Редактор скриншотов: карандаш, маркер, стрелка, рамка, текст, пикселизация и обрезка.
// Всё рисуется на canvas в размере исходной картинки; правки хранятся списком,
// поэтому их можно отменять (Ctrl+Z) и возвращать (Ctrl+Y).

(function () {
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => [...r.querySelectorAll(s)];

  const COLORS = ["#ff5c5c", "#ffd43b", "#5ad17a", "#6cc4ff", "#ffffff", "#16181c"];
  const SIZES = { s: 1, m: 2, l: 3.5 };
  const KEYS = { KeyP: "pen", KeyH: "marker", KeyA: "arrow", KeyR: "rect", KeyT: "text", KeyB: "pixel", KeyC: "crop" };

  const root = $("#editor");
  const view = $("#ed-canvas");
  const vctx = view.getContext("2d");
  const wrap = $("#ed-stage");
  const textInput = $("#ed-text");

  const E = {
    open: false,
    img: null, // исходник (Image или canvas)
    W: 0,
    H: 0,
    full: null, // исходник + все правки, кроме обрезки
    fctx: null,
    ops: [],
    redo: [],
    tool: "pen",
    color: COLORS[0],
    size: "m",
    cur: null, // правка, которую сейчас рисуют
    cropDraft: null, // рамка обрезки в режиме обрезки
    text: null, // { x, y } — где вводится текст
    onSave: null,
    onClose: null,
  };

  // единица толщины — от размера картинки, чтобы на 4K линии не были тоньше, чем на 1080p
  const unit = () => Math.max(E.W, E.H) / 1920;
  const lineWidth = (op) => (op.type === "marker" ? 16 : 4) * SIZES[op.size] * unit();
  const fontSize = (op) => Math.round(26 * SIZES[op.size] * unit());

  function crop() {
    for (let i = E.ops.length - 1; i >= 0; i--) if (E.ops[i].type === "crop") return E.ops[i];
    return { x: 0, y: 0, w: E.W, h: E.H };
  }

  // ---------- рисование правок ----------
  function drawOp(ctx, op) {
    ctx.save();
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    ctx.strokeStyle = op.color;
    ctx.fillStyle = op.color;
    switch (op.type) {
      case "pen":
      case "marker": {
        const p = op.points;
        if (!p.length) break;
        if (op.type === "marker") {
          ctx.globalAlpha = 0.38;
          ctx.lineCap = "square";
        }
        ctx.lineWidth = lineWidth(op);
        ctx.beginPath();
        ctx.moveTo(p[0][0], p[0][1]);
        if (p.length === 1) ctx.lineTo(p[0][0] + 0.01, p[0][1]);
        // сглаживание: кривые через середины соседних точек
        for (let i = 1; i < p.length - 1; i++) {
          const mx = (p[i][0] + p[i + 1][0]) / 2;
          const my = (p[i][1] + p[i + 1][1]) / 2;
          ctx.quadraticCurveTo(p[i][0], p[i][1], mx, my);
        }
        if (p.length > 1) ctx.lineTo(p[p.length - 1][0], p[p.length - 1][1]);
        ctx.stroke();
        break;
      }
      case "arrow": {
        const lw = lineWidth(op);
        const dx = op.x2 - op.x1, dy = op.y2 - op.y1;
        const len = Math.hypot(dx, dy);
        if (len < 1) break;
        const head = Math.min(len * 0.6, lw * 4.5);
        const a = Math.atan2(dy, dx);
        const bx = op.x2 - Math.cos(a) * head * 0.8, by = op.y2 - Math.sin(a) * head * 0.8;
        ctx.lineWidth = lw;
        ctx.beginPath();
        ctx.moveTo(op.x1, op.y1);
        ctx.lineTo(bx, by);
        ctx.stroke();
        ctx.beginPath();
        ctx.moveTo(op.x2, op.y2);
        ctx.lineTo(op.x2 - Math.cos(a - 0.45) * head, op.y2 - Math.sin(a - 0.45) * head);
        ctx.lineTo(op.x2 - Math.cos(a + 0.45) * head, op.y2 - Math.sin(a + 0.45) * head);
        ctx.closePath();
        ctx.fill();
        break;
      }
      case "rect": {
        const r = norm(op);
        ctx.lineWidth = lineWidth(op);
        ctx.beginPath();
        ctx.roundRect(r.x, r.y, r.w, r.h, lineWidth(op));
        ctx.stroke();
        break;
      }
      case "text": {
        const fs = fontSize(op);
        ctx.font = `600 ${fs}px "Segoe UI", system-ui, sans-serif`;
        ctx.textBaseline = "top";
        // тёмная обводка — текст читается на любом фоне
        ctx.lineWidth = Math.max(2, fs / 7);
        ctx.strokeStyle = op.color === "#16181c" ? "#ffffff" : "rgba(0,0,0,.85)";
        op.text.split("\n").forEach((line, i) => {
          ctx.strokeText(line, op.x, op.y + i * fs * 1.2);
          ctx.fillText(line, op.x, op.y + i * fs * 1.2);
        });
        break;
      }
      case "pixel": {
        const r = clampRect(norm(op));
        if (r.w < 2 || r.h < 2) break;
        const block = Math.max(6, Math.round(14 * unit() * SIZES[op.size]));
        const sw = Math.max(1, Math.round(r.w / block)), sh = Math.max(1, Math.round(r.h / block));
        const tmp = document.createElement("canvas");
        tmp.width = sw;
        tmp.height = sh;
        const t = tmp.getContext("2d");
        t.imageSmoothingEnabled = true;
        t.drawImage(ctx.canvas, r.x, r.y, r.w, r.h, 0, 0, sw, sh);
        ctx.imageSmoothingEnabled = false;
        ctx.drawImage(tmp, 0, 0, sw, sh, r.x, r.y, r.w, r.h);
        break;
      }
    }
    ctx.restore();
  }

  const norm = (op) => ({
    x: Math.min(op.x1, op.x2),
    y: Math.min(op.y1, op.y2),
    w: Math.abs(op.x2 - op.x1),
    h: Math.abs(op.y2 - op.y1),
  });
  function clampRect(r) {
    const x = Math.max(0, Math.round(r.x)), y = Math.max(0, Math.round(r.y));
    return { x, y, w: Math.min(E.W, Math.round(r.x + r.w)) - x, h: Math.min(E.H, Math.round(r.y + r.h)) - y };
  }

  // полная перерисовка — после отмены; обычные правки дорисовываются поверх
  function rebuild() {
    E.fctx.clearRect(0, 0, E.W, E.H);
    E.fctx.drawImage(E.img, 0, 0, E.W, E.H);
    for (const op of E.ops) if (op.type !== "crop") drawOp(E.fctx, op);
  }

  function render() {
    const cropping = E.tool === "crop";
    const c = cropping ? { x: 0, y: 0, w: E.W, h: E.H } : crop();
    if (view.width !== c.w || view.height !== c.h) {
      view.width = c.w;
      view.height = c.h;
      fit();
    }
    vctx.clearRect(0, 0, c.w, c.h);
    vctx.drawImage(E.full, c.x, c.y, c.w, c.h, 0, 0, c.w, c.h);
    if (E.cur && E.cur.type !== "pixel") {
      vctx.save();
      vctx.translate(-c.x, -c.y);
      drawOp(vctx, E.cur);
      vctx.restore();
    }
    if (E.cur && E.cur.type === "pixel") {
      // предпросмотр пикселизации прямо на картинке
      // (пикселизация читает пиксели холста, поэтому координаты сдвигаем в саму правку)
      const op = { ...E.cur, x1: E.cur.x1 - c.x, x2: E.cur.x2 - c.x, y1: E.cur.y1 - c.y, y2: E.cur.y2 - c.y };
      const r = norm(op);
      vctx.save();
      drawOp(vctx, op);
      vctx.setLineDash([8 * unit(), 6 * unit()]);
      vctx.lineWidth = 2 * unit();
      vctx.strokeStyle = "rgba(255,255,255,.8)";
      vctx.strokeRect(r.x, r.y, r.w, r.h);
      vctx.restore();
    }
    if (cropping) {
      const r = E.cropDraft || crop();
      vctx.save();
      vctx.fillStyle = "rgba(0,0,0,.55)";
      vctx.beginPath();
      vctx.rect(0, 0, E.W, E.H);
      vctx.rect(r.x, r.y, r.w, r.h);
      vctx.fill("evenodd");
      vctx.strokeStyle = "#6cc4ff";
      vctx.lineWidth = 2 * unit();
      vctx.strokeRect(r.x, r.y, r.w, r.h);
      vctx.restore();
    }
    renderBar();
  }

  // картинка вписывается в область просмотра
  function fit() {
    const box = wrap.getBoundingClientRect();
    const k = Math.min(box.width / view.width, box.height / view.height, 1);
    view.style.width = `${Math.floor(view.width * k)}px`;
    view.style.height = `${Math.floor(view.height * k)}px`;
  }
  window.addEventListener("resize", () => E.open && fit());

  // экранные координаты -> координаты картинки
  function toImage(e) {
    const b = view.getBoundingClientRect();
    const c = E.tool === "crop" ? { x: 0, y: 0 } : crop();
    return [((e.clientX - b.left) / b.width) * view.width + c.x, ((e.clientY - b.top) / b.height) * view.height + c.y];
  }

  // ---------- мышь ----------
  view.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    if (E.text) commitText();
    const [x, y] = toImage(e);
    const base = { color: E.color, size: E.size };
    switch (E.tool) {
      case "pen":
      case "marker":
        E.cur = { type: E.tool, ...base, points: [[x, y]] };
        break;
      case "arrow":
      case "rect":
      case "pixel":
        E.cur = { type: E.tool, ...base, x1: x, y1: y, x2: x, y2: y };
        break;
      case "crop":
        E.cropStart = [x, y];
        E.cropDraft = { x, y, w: 0, h: 0 };
        break;
      case "text":
        e.preventDefault();
        startText(x, y, e);
        return;
    }
    view.setPointerCapture(e.pointerId);
    render();
  });

  view.addEventListener("pointermove", (e) => {
    if (!E.cur && !E.cropStart) return;
    const [x, y] = toImage(e);
    if (E.cur && (E.cur.type === "pen" || E.cur.type === "marker")) {
      const last = E.cur.points[E.cur.points.length - 1];
      if (Math.hypot(x - last[0], y - last[1]) < unit()) return;
      if (E.cur.type === "marker" && e.shiftKey) {
        // Shift — ровная горизонтальная линия, удобно выделять текст
        E.cur.points = [E.cur.points[0], [x, E.cur.points[0][1]]];
      } else E.cur.points.push([x, y]);
    } else if (E.cur) {
      E.cur.x2 = x;
      E.cur.y2 = y;
    } else {
      const [sx, sy] = E.cropStart;
      E.cropDraft = clampRect(norm({ x1: sx, y1: sy, x2: x, y2: y }));
    }
    render();
  });

  function endPointer() {
    if (E.cur) {
      const op = E.cur;
      E.cur = null;
      const tiny = (op.type === "arrow" || op.type === "rect" || op.type === "pixel") && Math.hypot(op.x2 - op.x1, op.y2 - op.y1) < 4 * unit();
      if (!tiny) push(op);
      else render();
    }
    if (E.cropStart) {
      E.cropStart = null;
      if (E.cropDraft && (E.cropDraft.w < 16 || E.cropDraft.h < 16)) E.cropDraft = null;
      render();
    }
  }
  view.addEventListener("pointerup", endPointer);
  view.addEventListener("pointercancel", endPointer);

  function push(op) {
    E.ops.push(op);
    E.redo = [];
    if (op.type !== "crop") drawOp(E.fctx, op);
    render();
  }

  // ---------- текст ----------
  function startText(x, y) {
    E.text = { x, y };
    const b = view.getBoundingClientRect();
    const k = b.width / view.width;
    const c = crop();
    const fs = fontSize({ size: E.size });
    textInput.hidden = false;
    textInput.value = "";
    textInput.style.left = `${b.left + (x - c.x) * k}px`;
    textInput.style.top = `${b.top + (y - c.y) * k}px`;
    textInput.style.fontSize = `${fs * k}px`;
    textInput.style.color = E.color;
    autoSize();
    setTimeout(() => textInput.focus(), 0);
  }
  function autoSize() {
    textInput.style.height = "auto";
    textInput.style.height = `${textInput.scrollHeight}px`;
    textInput.style.width = "auto";
    textInput.style.width = `${Math.max(40, textInput.scrollWidth + 8)}px`;
  }
  textInput.addEventListener("input", autoSize);
  textInput.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Escape") cancelText();
    else if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      commitText();
    }
  });
  textInput.addEventListener("blur", () => setTimeout(() => E.text && commitText(), 0));
  function commitText() {
    const t = textInput.value.trim();
    const pos = E.text;
    cancelText();
    if (t && pos) push({ type: "text", color: E.color, size: E.size, text: t, x: pos.x, y: pos.y });
  }
  function cancelText() {
    E.text = null;
    textInput.hidden = true;
    textInput.blur();
  }

  // ---------- обрезка ----------
  function applyCrop() {
    const d = E.cropDraft;
    E.cropDraft = null;
    const cur = crop();
    if (d && (d.x !== cur.x || d.y !== cur.y || d.w !== cur.w || d.h !== cur.h)) {
      E.ops.push({ type: "crop", ...d });
      E.redo = [];
    }
    setTool("pen");
  }

  // ---------- отмена ----------
  function undo() {
    if (!E.ops.length) return;
    E.redo.push(E.ops.pop());
    rebuild();
    render();
  }
  function redo() {
    if (!E.redo.length) return;
    const op = E.redo.pop();
    E.ops.push(op);
    if (op.type !== "crop") drawOp(E.fctx, op);
    render();
  }

  // ---------- панель ----------
  function setTool(t) {
    if (E.text) commitText();
    if (E.tool === "crop" && t !== "crop") E.cropDraft = null;
    E.tool = t;
    if (t === "crop") E.cropDraft = { ...crop() };
    root.dataset.tool = t;
    render();
  }

  function renderBar() {
    $$("#ed-tools [data-tool]").forEach((b) => b.classList.toggle("active", b.dataset.tool === E.tool));
    $$("#ed-colors button").forEach((b) => b.classList.toggle("active", b.dataset.color === E.color));
    $$("#ed-sizes button").forEach((b) => b.classList.toggle("active", b.dataset.size === E.size));
    $("#ed-undo").disabled = !E.ops.length;
    $("#ed-redo").disabled = !E.redo.length;
    const cropping = E.tool === "crop";
    $("#ed-crop-bar").hidden = !cropping;
    $("#ed-save-bar").hidden = cropping;
    const c = cropping ? E.cropDraft || crop() : crop();
    $("#ed-size-info").textContent = `${Math.round(c.w)}×${Math.round(c.h)}`;
    const dirty = E.ops.length > 0;
    $("#ed-save-copy").disabled = !dirty;
    $("#ed-save-replace").disabled = !dirty;
  }

  $("#ed-colors").innerHTML = COLORS.map((c) => `<button data-color="${c}" style="--c:${c}" title="${c}"></button>`).join("");
  $$("#ed-tools [data-tool]").forEach((b) => (b.onclick = () => setTool(b.dataset.tool)));
  $$("#ed-colors button").forEach((b) =>
    (b.onclick = () => {
      E.color = b.dataset.color;
      if (E.text) textInput.style.color = E.color;
      renderBar();
    })
  );
  $$("#ed-sizes button").forEach((b) => (b.onclick = () => ((E.size = b.dataset.size), renderBar())));
  $("#ed-undo").onclick = undo;
  $("#ed-redo").onclick = redo;
  $("#ed-crop-apply").onclick = applyCrop;
  $("#ed-crop-reset").onclick = () => {
    E.cropDraft = { x: 0, y: 0, w: E.W, h: E.H };
    render();
  };
  $("#ed-crop-cancel").onclick = () => setTool("pen");
  $("#ed-cancel").onclick = () => close();
  $("#ed-save-copy").onclick = () => save(false);
  $("#ed-save-replace").onclick = () => save(true);

  // клавиши редактора перехватываются раньше клавиш плеера (иначе Delete удалил бы клип)
  window.addEventListener(
    "keydown",
    (e) => {
      if (!E.open || e.target === textInput) return;
      if ($("#confirm").classList.contains("open")) return;
      e.stopImmediatePropagation();
      const ctrl = e.ctrlKey || e.metaKey;
      if (ctrl && e.code === "KeyZ" && !e.shiftKey) return e.preventDefault(), undo();
      if (ctrl && (e.code === "KeyY" || (e.code === "KeyZ" && e.shiftKey))) return e.preventDefault(), redo();
      if (ctrl && e.code === "KeyS") return e.preventDefault(), save(false);
      if (e.key === "Escape") return E.tool === "crop" ? setTool("pen") : close();
      if (e.key === "Enter" && E.tool === "crop") return applyCrop();
      if (!ctrl && !e.altKey && KEYS[e.code]) setTool(KEYS[e.code]);
      if (!ctrl && e.code >= "Digit1" && e.code <= "Digit6") {
        E.color = COLORS[+e.code.slice(5) - 1];
        renderBar();
      }
    },
    true
  );

  // ---------- открыть / сохранить ----------
  async function open(src, opts) {
    E.img = src;
    E.W = src.naturalWidth || src.width;
    E.H = src.naturalHeight || src.height;
    E.full = document.createElement("canvas");
    E.full.width = E.W;
    E.full.height = E.H;
    E.fctx = E.full.getContext("2d");
    E.ops = [];
    E.redo = [];
    E.cur = null;
    E.cropDraft = null;
    E.onSave = opts.onSave;
    E.onClose = opts.onClose;
    $("#ed-title").textContent = opts.title || "";
    rebuild();
    root.hidden = false;
    E.open = true;
    view.width = 0; // заставит render пересчитать размер
    setTool(E.tool === "crop" ? "pen" : E.tool);
    requestAnimationFrame(fit);
  }

  async function close(force = false) {
    if (!E.open) return;
    if (E.text) commitText();
    if (!force && E.ops.length && !(await window.confirmBox("Закрыть без сохранения?", "Правки на этом скриншоте пропадут.", "Закрыть"))) return;
    E.open = false;
    root.hidden = true;
    E.img = null;
    E.full = null;
    if (E.onClose) E.onClose();
  }

  // готовая картинка (с обрезкой) в PNG, base64
  function exportPng() {
    const c = crop();
    const out = document.createElement("canvas");
    out.width = c.w;
    out.height = c.h;
    out.getContext("2d").drawImage(E.full, c.x, c.y, c.w, c.h, 0, 0, c.w, c.h);
    return new Promise((res, rej) =>
      out.toBlob((b) => {
        if (!b) return rej("не удалось собрать картинку");
        const r = new FileReader();
        r.onload = () => res(String(r.result).split(",")[1]);
        r.onerror = () => rej("не удалось прочитать картинку");
        r.readAsDataURL(b);
      }, "image/png")
    );
  }

  async function save(replace) {
    if (!E.ops.length || !E.onSave) return;
    if (E.text) commitText();
    if (replace && !(await window.confirmBox("Заменить оригинал?", "Исходный скриншот будет перезаписан, вернуть его не получится.", "Заменить"))) return;
    root.classList.add("busy");
    try {
      const png = await exportPng();
      await E.onSave(png, replace);
      close(true);
    } catch (e) {
      // сообщение об ошибке показывает вызывающий код
    } finally {
      root.classList.remove("busy");
    }
  }

  window.ImageEditor = { open, close, isOpen: () => E.open };
})();
