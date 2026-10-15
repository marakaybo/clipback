// Редактор скриншотов: карандаш, маркер, стрелка, рамка, текст, пикселизация и обрезка.
// Всё рисуется на canvas в размере исходной картинки. Правки — список объектов,
// которые можно выделять, двигать, менять и удалять; история — снимки этого списка
// (Ctrl+Z / Ctrl+Y). Объекты не меняются на месте, поэтому снимок — просто копия массива.

(function () {
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => [...r.querySelectorAll(s)];

  const COLORS = ["#ff5c5c", "#ffd43b", "#5ad17a", "#6cc4ff", "#ffffff", "#16181c"];
  const SIZES = { s: 1, m: 2, l: 3.5 };
  const KEYS = { KeyV: "move", KeyP: "pen", KeyH: "marker", KeyA: "arrow", KeyR: "rect", KeyT: "text", KeyB: "pixel", KeyC: "crop" };
  const HINTS = {
    move: "Перетащите объект, чтобы сдвинуть. Двойной щелчок по тексту — изменить, Delete — удалить",
    pen: "Рисуйте мышью",
    marker: "Проведите по важному. С Shift — ровная линия",
    arrow: "Потяните от начала к острию",
    rect: "Обведите область",
    text: "Щёлкните, где нужен текст. Готовый текст можно перетащить, двойной щелчок — изменить",
    pixel: "Выделите то, что нужно скрыть: ник, чат, личные данные",
    crop: "Выделите нужную область и нажмите «Обрезать»",
  };

  const root = $("#editor");
  const view = $("#ed-canvas");
  const vctx = view.getContext("2d");
  const wrap = $("#ed-stage");
  const textInput = $("#ed-text");
  const measure = document.createElement("canvas").getContext("2d");

  const E = {
    open: false,
    img: null,
    W: 0,
    H: 0,
    full: null, // исходник + объекты (без обрезки)
    fctx: null,
    ops: [],
    undo: [],
    redo: [],
    tool: "pen",
    color: COLORS[0],
    size: "m",
    cur: null, // объект, который сейчас рисуют
    sel: -1, // выделенный объект
    drag: null, // { i, from: [x, y], orig, moved }
    cropDraft: null,
    cropStart: null,
    text: null, // { x, y, edit: индекс или -1 }
    onSave: null,
    onClose: null,
  };

  const unit = () => Math.max(E.W, E.H) / 1920;
  const lineWidth = (op) => (op.type === "marker" ? 16 : 4) * SIZES[op.size] * unit();
  const fontSize = (op) => Math.round(26 * SIZES[op.size] * unit());
  const font = (fs) => `600 ${fs}px "Segoe UI", system-ui, sans-serif`;

  function crop() {
    for (let i = E.ops.length - 1; i >= 0; i--) if (E.ops[i].type === "crop") return E.ops[i];
    return { x: 0, y: 0, w: E.W, h: E.H };
  }
  const isObject = (op) => op.type !== "crop";

  // ---------- геометрия объектов ----------
  const norm = (op) => ({ x: Math.min(op.x1, op.x2), y: Math.min(op.y1, op.y2), w: Math.abs(op.x2 - op.x1), h: Math.abs(op.y2 - op.y1) });
  function clampRect(r) {
    const x = Math.max(0, Math.round(r.x)), y = Math.max(0, Math.round(r.y));
    return { x, y, w: Math.min(E.W, Math.round(r.x + r.w)) - x, h: Math.min(E.H, Math.round(r.y + r.h)) - y };
  }

  function shift(op, dx, dy) {
    switch (op.type) {
      case "pen":
      case "marker":
        return { ...op, points: op.points.map(([x, y]) => [x + dx, y + dy]) };
      case "text":
        return { ...op, x: op.x + dx, y: op.y + dy };
      default:
        return { ...op, x1: op.x1 + dx, x2: op.x2 + dx, y1: op.y1 + dy, y2: op.y2 + dy };
    }
  }

  function textBox(op) {
    const fs = fontSize(op);
    measure.font = font(fs);
    const lines = op.text.split("\n");
    const w = Math.max(...lines.map((l) => measure.measureText(l).width));
    return { x: op.x, y: op.y, w, h: lines.length * fs * 1.2 };
  }

  function bbox(op) {
    switch (op.type) {
      case "pen":
      case "marker": {
        const xs = op.points.map((p) => p[0]), ys = op.points.map((p) => p[1]);
        const pad = lineWidth(op) / 2;
        const x = Math.min(...xs) - pad, y = Math.min(...ys) - pad;
        return { x, y, w: Math.max(...xs) + pad - x, h: Math.max(...ys) + pad - y };
      }
      case "text":
        return textBox(op);
      case "arrow": {
        const pad = lineWidth(op) * 2.5;
        const r = norm(op);
        return { x: r.x - pad, y: r.y - pad, w: r.w + pad * 2, h: r.h + pad * 2 };
      }
      default: {
        const r = norm(op);
        const pad = op.type === "rect" ? lineWidth(op) / 2 : 0;
        return { x: r.x - pad, y: r.y - pad, w: r.w + pad * 2, h: r.h + pad * 2 };
      }
    }
  }

  function segDist(px, py, ax, ay, bx, by) {
    const dx = bx - ax, dy = by - ay;
    const l2 = dx * dx + dy * dy;
    const t = l2 ? Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / l2)) : 0;
    return Math.hypot(px - ax - t * dx, py - ay - t * dy);
  }

  function hits(op, x, y) {
    const tol = 8 * unit();
    const inBox = (b, pad = 0) => x >= b.x - pad && x <= b.x + b.w + pad && y >= b.y - pad && y <= b.y + b.h + pad;
    switch (op.type) {
      case "pen":
      case "marker": {
        const p = op.points, lim = lineWidth(op) / 2 + tol;
        if (p.length === 1) return Math.hypot(x - p[0][0], y - p[0][1]) < lim;
        for (let i = 1; i < p.length; i++) if (segDist(x, y, p[i - 1][0], p[i - 1][1], p[i][0], p[i][1]) < lim) return true;
        return false;
      }
      case "arrow":
        return segDist(x, y, op.x1, op.y1, op.x2, op.y2) < lineWidth(op) * 1.5 + tol;
      case "text":
        return inBox(textBox(op), tol);
      default:
        return inBox(bbox(op), tol);
    }
  }

  // верхний объект под точкой (только подходящие типы)
  function hitTest(x, y, types) {
    for (let i = E.ops.length - 1; i >= 0; i--) {
      const op = E.ops[i];
      if (isObject(op) && (!types || types.includes(op.type)) && hits(op, x, y)) return i;
    }
    return -1;
  }

  // ---------- рисование ----------
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
          ctx.quadraticCurveTo(p[i][0], p[i][1], (p[i][0] + p[i + 1][0]) / 2, (p[i][1] + p[i + 1][1]) / 2);
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
        ctx.lineWidth = lw;
        ctx.beginPath();
        ctx.moveTo(op.x1, op.y1);
        ctx.lineTo(op.x2 - Math.cos(a) * head * 0.8, op.y2 - Math.sin(a) * head * 0.8);
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
        ctx.font = font(fs);
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
        // читает пиксели самого холста — координаты должны быть в системе этого холста
        const r = clampRect(norm(op));
        if (r.w < 2 || r.h < 2) break;
        const block = Math.max(6, Math.round(14 * unit() * SIZES[op.size]));
        const sw = Math.max(1, Math.round(r.w / block)), sh = Math.max(1, Math.round(r.h / block));
        const tmp = document.createElement("canvas");
        tmp.width = sw;
        tmp.height = sh;
        tmp.getContext("2d").drawImage(ctx.canvas, r.x, r.y, r.w, r.h, 0, 0, sw, sh);
        ctx.imageSmoothingEnabled = false;
        ctx.drawImage(tmp, 0, 0, sw, sh, r.x, r.y, r.w, r.h);
        break;
      }
    }
    ctx.restore();
  }

  // полная перерисовка; skip — объект, который сейчас перетаскивают (он рисуется поверх)
  function rebuild(skip = -1) {
    E.fctx.clearRect(0, 0, E.W, E.H);
    E.fctx.drawImage(E.img, 0, 0, E.W, E.H);
    E.ops.forEach((op, i) => i !== skip && isObject(op) && drawOp(E.fctx, op));
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
    // объект, который рисуют или тащат, — поверх (в координатах видимой области)
    const floating = E.cur || (E.drag && E.drag.moved ? E.ops[E.drag.i] : null);
    if (floating) drawOp(vctx, shift(floating, -c.x, -c.y));
    if (E.cur && E.cur.type === "pixel") outline(norm(E.cur), c, "rgba(255,255,255,.85)");
    if (!cropping && E.sel >= 0 && E.ops[E.sel] && !E.text) outline(bbox(E.ops[E.sel]), c, "#6cc4ff");
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

  function outline(b, c, color) {
    const pad = 4 * unit();
    vctx.save();
    vctx.setLineDash([7 * unit(), 5 * unit()]);
    vctx.lineWidth = 1.5 * unit();
    vctx.strokeStyle = color;
    vctx.strokeRect(b.x - c.x - pad, b.y - c.y - pad, b.w + pad * 2, b.h + pad * 2);
    vctx.restore();
  }

  function fit() {
    const box = wrap.getBoundingClientRect();
    const k = Math.min(box.width / view.width, box.height / view.height, 1);
    view.style.width = `${Math.floor(view.width * k)}px`;
    view.style.height = `${Math.floor(view.height * k)}px`;
  }
  window.addEventListener("resize", () => E.open && fit());

  function toImage(e) {
    const b = view.getBoundingClientRect();
    const c = E.tool === "crop" ? { x: 0, y: 0 } : crop();
    return [((e.clientX - b.left) / b.width) * view.width + c.x, ((e.clientY - b.top) / b.height) * view.height + c.y];
  }

  // ---------- история ----------
  function commit(ops) {
    E.undo.push(E.ops);
    E.redo = [];
    E.ops = ops;
  }
  function add(op) {
    commit([...E.ops, op]);
    E.sel = -1;
    drawOp(E.fctx, op); // новый объект — сверху, перерисовывать всё не нужно
    render();
  }
  function replace(i, op) {
    const ops = E.ops.slice();
    if (op) ops[i] = op;
    else ops.splice(i, 1);
    commit(ops);
    E.sel = op ? i : -1;
    rebuild();
    render();
  }
  function undo() {
    if (!E.undo.length) return;
    E.redo.push(E.ops);
    E.ops = E.undo.pop();
    E.sel = -1;
    rebuild();
    render();
  }
  function redo() {
    if (!E.redo.length) return;
    E.undo.push(E.ops);
    E.ops = E.redo.pop();
    E.sel = -1;
    rebuild();
    render();
  }

  // ---------- мышь ----------
  view.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    if (E.text) return commitText();
    const [x, y] = toImage(e);

    // «Выбор» берёт любой объект, «Текст» — готовый текст
    const grab = E.tool === "move" ? hitTest(x, y) : E.tool === "text" ? hitTest(x, y, ["text"]) : -1;
    if (grab >= 0) {
      E.sel = grab;
      E.drag = { i: grab, from: [x, y], orig: E.ops[grab], moved: false };
      view.setPointerCapture(e.pointerId);
      render();
      return;
    }
    if (E.tool === "move") {
      E.sel = -1;
      return render();
    }

    E.sel = -1;
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
        startText(x, y, -1);
        return;
    }
    view.setPointerCapture(e.pointerId);
    render();
  });

  view.addEventListener("dblclick", (e) => {
    if (E.tool === "crop") return;
    const [x, y] = toImage(e);
    const i = hitTest(x, y, ["text"]);
    if (i >= 0) {
      e.preventDefault();
      E.drag = null;
      startText(E.ops[i].x, E.ops[i].y, i);
    }
  });

  view.addEventListener("pointermove", (e) => {
    const [x, y] = toImage(e);
    if (E.drag) {
      const d = E.drag;
      const dx = x - d.from[0], dy = y - d.from[1];
      if (!d.moved && Math.hypot(dx, dy) < 3 * unit()) return;
      if (!d.moved) {
        d.moved = true;
        rebuild(d.i); // под перетаскиваемым — картинка без него
      }
      E.ops[d.i] = shift(d.orig, dx, dy); // временно; в историю попадёт по отпусканию
      return render();
    }
    if (!E.cur && !E.cropStart) {
      // курсор подсказывает, что объект можно взять
      const can = (E.tool === "move" && hitTest(x, y) >= 0) || (E.tool === "text" && hitTest(x, y, ["text"]) >= 0);
      view.style.cursor = can ? "move" : "";
      return;
    }
    if (E.cur && (E.cur.type === "pen" || E.cur.type === "marker")) {
      const last = E.cur.points[E.cur.points.length - 1];
      if (Math.hypot(x - last[0], y - last[1]) < unit()) return;
      if (E.cur.type === "marker" && e.shiftKey) E.cur.points = [E.cur.points[0], [x, E.cur.points[0][1]]];
      else E.cur.points.push([x, y]);
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
    if (E.drag) {
      const d = E.drag;
      E.drag = null;
      if (d.moved) {
        const moved = E.ops[d.i];
        E.ops[d.i] = d.orig; // вернуть, чтобы в истории остался прежний вариант
        E.ops = E.ops.slice();
        replace(d.i, moved);
      } else render();
      return;
    }
    if (E.cur) {
      const op = E.cur;
      E.cur = null;
      const tiny = (op.type === "arrow" || op.type === "rect" || op.type === "pixel") && Math.hypot(op.x2 - op.x1, op.y2 - op.y1) < 4 * unit();
      if (!tiny) add(op);
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

  // ---------- текст ----------
  function startText(x, y, edit) {
    const op = edit >= 0 ? E.ops[edit] : null;
    E.text = { x, y, edit };
    E.sel = -1;
    if (op) {
      // на время правки текст убирается с картинки — его заменяет поле ввода
      rebuild(edit);
      E.color = op.color;
      E.size = op.size;
    }
    const b = view.getBoundingClientRect();
    const k = b.width / view.width;
    const c = crop();
    const fs = fontSize({ size: E.size });
    textInput.hidden = false;
    textInput.value = op ? op.text : "";
    textInput.style.left = `${b.left + (x - c.x) * k}px`;
    textInput.style.top = `${b.top + (y - c.y) * k}px`;
    textInput.style.fontSize = `${fs * k}px`;
    textInput.style.color = E.color;
    autoSize();
    render();
    setTimeout(() => {
      textInput.focus();
      textInput.select();
    }, 0);
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
    const t = textInput.value.replace(/\s+$/, "");
    const st = E.text;
    closeText();
    if (!st) return;
    if (st.edit >= 0) {
      const old = E.ops[st.edit];
      if (!t.trim()) replace(st.edit, null);
      else if (t !== old.text || old.color !== E.color || old.size !== E.size) replace(st.edit, { ...old, text: t, color: E.color, size: E.size });
      else {
        rebuild();
        render();
      }
    } else if (t.trim()) add({ type: "text", color: E.color, size: E.size, text: t, x: st.x, y: st.y });
    else render();
  }
  function cancelText() {
    const st = E.text;
    closeText();
    if (st && st.edit >= 0) rebuild();
    render();
  }
  function closeText() {
    E.text = null;
    textInput.hidden = true;
    textInput.blur();
  }

  // ---------- обрезка ----------
  function applyCrop() {
    const d = E.cropDraft;
    E.cropDraft = null;
    const cur = crop();
    if (d && (d.x !== cur.x || d.y !== cur.y || d.w !== cur.w || d.h !== cur.h)) commit([...E.ops, { type: "crop", ...d }]);
    setTool("move");
  }

  // ---------- панель ----------
  function setTool(t) {
    if (E.text) commitText();
    if (E.tool === "crop" && t !== "crop") E.cropDraft = null;
    E.tool = t;
    if (t !== "move" && t !== "text") E.sel = -1;
    if (t === "crop") E.cropDraft = { ...crop() };
    root.dataset.tool = t;
    view.style.cursor = "";
    render();
  }

  // цвет и толщина меняют и выделенный объект
  function applyStyle(patch) {
    Object.assign(E, patch);
    if (E.text) {
      textInput.style.color = E.color;
      const b = view.getBoundingClientRect();
      textInput.style.fontSize = `${fontSize({ size: E.size }) * (b.width / view.width)}px`;
      autoSize();
    } else if (E.sel >= 0 && E.ops[E.sel]) {
      const op = E.ops[E.sel];
      if (op.color !== E.color || op.size !== E.size) return replace(E.sel, { ...op, color: E.color, size: E.size });
    }
    renderBar();
  }

  function renderBar() {
    $$("#ed-tools [data-tool]").forEach((b) => b.classList.toggle("active", b.dataset.tool === E.tool));
    $$("#ed-colors button").forEach((b) => b.classList.toggle("active", b.dataset.color === E.color));
    $$("#ed-sizes button").forEach((b) => b.classList.toggle("active", b.dataset.size === E.size));
    $("#ed-undo").disabled = !E.undo.length;
    $("#ed-redo").disabled = !E.redo.length;
    $("#ed-delete").hidden = !(E.sel >= 0 && E.ops[E.sel]);
    const cropping = E.tool === "crop";
    $("#ed-crop-bar").hidden = !cropping;
    $("#ed-save-bar").hidden = cropping;
    const c = cropping ? E.cropDraft || crop() : crop();
    $("#ed-size-info").textContent = `${Math.round(c.w)}×${Math.round(c.h)}`;
    $("#ed-hint").textContent = HINTS[E.tool] || "";
    $("#ed-hint").title = HINTS[E.tool] || "";
    const dirty = E.undo.length > 0;
    $("#ed-save-copy").disabled = !dirty;
    $("#ed-save-replace").disabled = !dirty;
  }

  $("#ed-colors").innerHTML = COLORS.map((c, i) => `<button data-color="${c}" style="--c:${c}" title="Цвет ${i + 1}"></button>`).join("");
  $$("#ed-tools [data-tool]").forEach((b) => (b.onclick = () => setTool(b.dataset.tool)));
  $$("#ed-colors button").forEach((b) => (b.onclick = () => applyStyle({ color: b.dataset.color })));
  $$("#ed-sizes button").forEach((b) => (b.onclick = () => applyStyle({ size: b.dataset.size })));
  $("#ed-undo").onclick = undo;
  $("#ed-redo").onclick = redo;
  $("#ed-delete").onclick = () => E.sel >= 0 && replace(E.sel, null);
  $("#ed-crop-apply").onclick = applyCrop;
  $("#ed-crop-reset").onclick = () => {
    E.cropDraft = { x: 0, y: 0, w: E.W, h: E.H };
    render();
  };
  $("#ed-crop-cancel").onclick = () => setTool("move");
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
      if (e.key === "Escape") {
        if (E.tool === "crop") return setTool("move");
        if (E.sel >= 0) return (E.sel = -1), render();
        return close();
      }
      if (e.key === "Enter" && E.tool === "crop") return applyCrop();
      if ((e.key === "Delete" || e.key === "Backspace") && E.sel >= 0) return e.preventDefault(), replace(E.sel, null);
      if (e.key === "Enter" && E.sel >= 0 && E.ops[E.sel].type === "text") return startText(E.ops[E.sel].x, E.ops[E.sel].y, E.sel);
      if (E.sel >= 0 && e.key.startsWith("Arrow")) {
        // сдвиг выделенного стрелками (с Shift — крупнее)
        e.preventDefault();
        const step = (e.shiftKey ? 10 : 1) * unit();
        const d = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] }[e.key];
        return replace(E.sel, shift(E.ops[E.sel], d[0], d[1]));
      }
      if (!ctrl && !e.altKey && KEYS[e.code]) setTool(KEYS[e.code]);
      if (!ctrl && e.code >= "Digit1" && e.code <= "Digit6") applyStyle({ color: COLORS[+e.code.slice(5) - 1] });
    },
    true
  );

  // ---------- открыть / сохранить ----------
  function open(src, opts) {
    E.img = src;
    E.W = src.naturalWidth || src.width;
    E.H = src.naturalHeight || src.height;
    E.full = document.createElement("canvas");
    E.full.width = E.W;
    E.full.height = E.H;
    E.fctx = E.full.getContext("2d");
    E.ops = [];
    E.undo = [];
    E.redo = [];
    E.cur = null;
    E.sel = -1;
    E.drag = null;
    E.cropDraft = null;
    E.onSave = opts.onSave;
    E.onClose = opts.onClose;
    rebuild();
    root.hidden = false;
    E.open = true;
    view.width = 0; // render пересчитает размер
    setTool(E.tool === "crop" || E.tool === "move" ? "pen" : E.tool);
    requestAnimationFrame(fit);
  }

  async function close(force = false) {
    if (!E.open) return;
    if (E.text) commitText();
    if (!force && E.undo.length && !(await window.confirmBox("Закрыть без сохранения?", "Правки на этом скриншоте пропадут.", "Закрыть"))) return;
    E.open = false;
    root.hidden = true;
    E.img = null;
    E.full = null;
    if (E.onClose) E.onClose();
  }

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

  async function save(replaceOriginal) {
    if (!E.undo.length || !E.onSave) return;
    if (E.text) commitText();
    if (replaceOriginal && !(await window.confirmBox("Заменить оригинал?", "Исходный скриншот будет перезаписан, вернуть его не получится.", "Заменить"))) return;
    E.sel = -1;
    rebuild();
    root.classList.add("busy");
    try {
      const png = await exportPng();
      await E.onSave(png, replaceOriginal);
      close(true);
    } catch (e) {
      // сообщение об ошибке показывает вызывающий код
    } finally {
      root.classList.remove("busy");
    }
  }

  window.ImageEditor = { open, close, isOpen: () => E.open };
})();
