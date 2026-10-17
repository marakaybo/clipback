// Виджеты оверлея в духе Xbox Game Bar: микшер громкости приложений с «замком»
// и нагрузка компьютера. Обновляются раз в секунду и только пока оверлей открыт.

(function () {
  const MODE = document.documentElement.dataset.mode;
  if (MODE !== "overlay" && MODE !== "browser") return;
  const root = document.getElementById("widgets");
  if (!root) return;

  const call = (cmd, args) => api.call(cmd, args);
  const el = (tag, cls, html) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (html != null) e.innerHTML = html;
    return e;
  };
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

  const ICON = {
    vol: '<svg viewBox="0 0 24 24"><path d="M4 9h4l5-4v14l-5-4H4z"/><path d="M16 9a4 4 0 0 1 0 6"/></svg>',
    mute: '<svg viewBox="0 0 24 24"><path d="M4 9h4l5-4v14l-5-4H4z"/><path d="M16 9l5 6M21 9l-5 6"/></svg>',
    lock: '<svg viewBox="0 0 24 24"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg>',
    unlock: '<svg viewBox="0 0 24 24"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 7.5-2"/></svg>',
  };

  root.innerHTML = `
    <section class="panel wg" id="wg-mixer">
      <div class="wg-head"><span>Звук</span><select class="wg-dev" id="mx-device" title="Устройство вывода"></select></div>
      <div class="mx-row mx-master" id="mx-master"></div>
      <div class="mx-list" id="mx-list"></div>
      <div id="mx-mic-box" hidden>
        <div class="wg-head wg-mic-head"><span>Микрофон</span><select class="wg-dev" id="mx-mic-device" title="Микрофон"></select></div>
        <div class="mx-row mx-master" id="mx-mic"></div>
      </div>
    </section>
    <section class="panel wg" id="wg-load">
      <div class="wg-head"><span>Система</span><span class="wg-sub" id="ld-gpu"></span></div>
      <div class="ld-grid" id="ld-grid"></div>
    </section>`;

  // ---------- микшер ----------
  const mixer = { rows: new Map(), busyUntil: new Map(), master: null };

  /// Ползунок 0–100 с заливкой до значения; вызовы громкости — не чаще раза в 40 мс.
  function slider(onSet, onDone) {
    const r = el("input");
    r.type = "range";
    r.min = 0;
    r.max = 100;
    let timer = null;
    let last = null;
    const paint = () => r.style.setProperty("--v", r.value + "%");
    r.addEventListener("input", () => {
      paint();
      last = r.value / 100;
      if (!timer) {
        timer = setTimeout(() => { timer = null; onSet(last); }, 40);
      }
    });
    r.addEventListener("change", () => onDone && onDone(r.value / 100));
    r.paint = paint;
    return r;
  }

  function busy(key, ms = 1500) {
    mixer.busyUntil.set(key, Date.now() + ms);
  }
  const isBusy = (key, input) => document.activeElement === input && input.matches(":active") || (mixer.busyUntil.get(key) || 0) > Date.now();

  /// kind: "master" — общая громкость, "mic" — микрофон, иначе — приложение.
  function makeRow(key, kind) {
    const master = !!kind;
    const volCmd = kind === "mic" ? "set_mic_volume" : master ? "set_master_volume" : "set_app_volume";
    const muteCmd = kind === "mic" ? "set_mic_mute" : master ? "set_master_mute" : "set_app_mute";
    const row = el("div", "mx-row" + (master ? " mx-master" : ""));
    const mute = el("button", "icon-btn xs mx-mute");
    const body = el("div", "mx-body");
    const top = el("div", "mx-top");
    const name = el("span", "mx-name");
    const val = el("span", "mx-val mono");
    top.append(name, val);
    const r = slider(
      (v) => { busy(key); val.textContent = Math.round(v * 100); call(volCmd, master ? { volume: v } : { key, volume: v }).catch(() => {}); },
      (v) => { busy(key); call(volCmd, master ? { volume: v } : { key, volume: v }).catch(() => {}); }
    );
    r.addEventListener("input", () => { val.textContent = r.value; });
    body.append(top, r);
    row.append(mute, body);
    const o = { row, mute, name, val, r, data: null, lock: null };
    mute.onclick = () => {
      const muted = !o.data.muted;
      o.data.muted = muted;
      busy(key);
      paintRow(o);
      call(muteCmd, master ? { muted } : { key, muted }).catch(() => {});
    };
    if (!master) {
      const lock = el("button", "icon-btn xs mx-lock");
      lock.onclick = () => {
        const on = !o.data.locked;
        o.data.locked = on;
        busy(key);
        paintRow(o);
        call("set_volume_lock", { key, name: o.data.name, volume: r.value / 100, on }).catch(() => {});
      };
      row.append(lock);
      o.lock = lock;
    }
    return o;
  }

  function paintRow(o) {
    const d = o.data;
    o.row.classList.toggle("muted-app", !!d.muted);
    o.row.classList.toggle("locked", !!d.locked);
    o.row.classList.toggle("idle", d.active === false);
    o.mute.innerHTML = d.muted ? ICON.mute : ICON.vol;
    o.mute.title = d.muted ? "Включить звук" : "Без звука";
    if (o.lock) {
      o.lock.innerHTML = d.locked ? ICON.lock : ICON.unlock;
      o.lock.title = d.locked ? "Громкость удерживается" : "Удерживать громкость";
    }
  }

  function fill(o, d, key) {
    // только что нажатые «без звука» и замок не перебиваем ответом, отправленным раньше
    if (o.data && (mixer.busyUntil.get(key) || 0) > Date.now()) {
      d.muted = o.data.muted;
      if ("locked" in o.data) d.locked = o.data.locked;
    }
    o.data = d;
    o.name.textContent = d.name;
    o.name.title = d.name;
    if (!isBusy(key, o.r)) {
      o.r.value = Math.round(d.volume * 100);
      o.r.paint();
      o.val.textContent = o.r.value;
    }
    paintRow(o);
  }

  /// Список устройств; выбор делает устройство устройством по умолчанию в Windows.
  /// Пока список открыт, его не перерисовываем.
  function fillDevices(sel, list, current) {
    if (document.activeElement === sel) return;
    const sig = list.map((d) => d.id + (d.isDefault ? "*" : "")).join("|") || current;
    if (sel.dataset.sig === sig) return;
    sel.dataset.sig = sig;
    sel.replaceChildren(...(list.length ? list : [{ id: "", name: current, isDefault: true }]).map((d) => new Option(d.name, d.id, d.isDefault, d.isDefault)));
    sel.disabled = list.length < 2;
    sel.title = current;
    sel.onchange = () => {
      sel.blur();
      call("set_default_device", { id: sel.value }).then(refreshMixer).catch(() => {});
    };
  }

  async function refreshMixer() {
    let m;
    try {
      m = await call("mixer_state");
    } catch (e) {
      const dev = document.getElementById("mx-device");
      dev.replaceChildren();
      dev.dataset.sig = "";
      document.getElementById("mx-list").innerHTML = `<div class="wg-empty">Нет устройства вывода</div>`;
      mixer.rows.clear();
      return;
    }
    fillDevices(document.getElementById("mx-device"), m.outputs || [], m.device);
    if (!mixer.master) {
      mixer.master = makeRow("@master", "master");
      document.getElementById("mx-master").replaceWith(mixer.master.row);
    }
    fill(mixer.master, { name: "Общая", volume: m.master, muted: m.masterMuted }, "@master");

    // микрофон по умолчанию: выбор, громкость, «без звука»
    document.getElementById("mx-mic-box").hidden = !m.mic;
    if (m.mic) {
      fillDevices(document.getElementById("mx-mic-device"), m.mics || [], m.mic.name);
      if (!mixer.mic) {
        mixer.mic = makeRow("@mic", "mic");
        document.getElementById("mx-mic").replaceWith(mixer.mic.row);
      }
      fill(mixer.mic, { name: "Громкость", volume: m.mic.volume, muted: m.mic.muted }, "@mic");
    }

    const list = document.getElementById("mx-list");
    const seen = new Set();
    m.apps.forEach((a, i) => {
      seen.add(a.key);
      let o = mixer.rows.get(a.key);
      if (!o) {
        o = makeRow(a.key, false);
        mixer.rows.set(a.key, o);
      }
      fill(o, a, a.key);
      if (list.children[i] !== o.row) list.insertBefore(o.row, list.children[i] || null);
    });
    for (const [key, o] of mixer.rows) {
      if (!seen.has(key)) {
        o.row.remove();
        mixer.rows.delete(key);
      }
    }
    if (!m.apps.length && !list.querySelector(".wg-empty")) list.innerHTML = `<div class="wg-empty">Сейчас ничего не играет</div>`;
    if (m.apps.length) list.querySelector(".wg-empty")?.remove();
  }

  // ---------- нагрузка ----------
  const HIST = 120; // секунд: история приходит из приложения, замер идёт в фоне
  const hist = { cpu: [], gpu: [] };
  const grid = document.getElementById("ld-grid");
  const tiles = {};
  for (const [id, label, spark] of [["cpu", "ЦП", true], ["gpu", "ГП", true], ["vram", "Видеопамять", false], ["ram", "ОЗУ", false]]) {
    const t = el("div", "ld-tile" + (spark ? "" : " wide"));
    t.innerHTML = `<div class="ld-top"><span class="ld-label">${label}</span><span class="ld-val mono"></span></div>` +
      (spark ? `<svg class="ld-spark" viewBox="0 0 ${HIST - 1} 20" preserveAspectRatio="none"><polyline/></svg>` : "") +
      `<div class="ld-bar"><i></i></div>`;
    grid.append(t);
    tiles[id] = { val: t.querySelector(".ld-val"), bar: t.querySelector(".ld-bar i"), line: t.querySelector("polyline"), tile: t };
  }
  const gb = (b) => (b / 1024 ** 3).toFixed(1).replace(".", ",");

  function setTile(id, pct, text) {
    const t = tiles[id];
    t.val.innerHTML = text;
    t.tile.classList.toggle("na", pct == null);
    t.bar.style.width = pct == null ? "0" : Math.max(0, Math.min(100, pct)) + "%";
    // память на исходе — жёлтым; видеокарта на 99 % в игре — это нормально
    t.tile.classList.toggle("hot", (id === "vram" || id === "ram") && pct != null && pct >= 90);
    if (t.line && hist[id].length) {
      const h = hist[id].slice(-HIST);
      const off = HIST - h.length;
      t.line.setAttribute("points", h.map((v, i) => `${off + i},${(20 - (v / 100) * 19).toFixed(1)}`).join(" "));
    }
  }

  async function refreshLoad() {
    let l;
    try {
      l = await call("system_load");
    } catch (e) {
      return;
    }
    if (!open) return;
    hist.cpu = l.cpuHistory || [];
    hist.gpu = l.gpuHistory || [];
    document.getElementById("ld-gpu").textContent = l.gpuName || "";
    document.getElementById("ld-gpu").title = l.gpuName || "";
    setTile("cpu", l.cpu, l.cpu == null ? "—" : `${Math.round(l.cpu)}%`);
    const temp = l.gpuTemp != null ? ` <small class="${l.gpuTemp >= 85 ? "warm" : ""}">${l.gpuTemp}°</small>` : "";
    setTile("gpu", l.gpu, (l.gpu == null ? "—" : `${Math.round(l.gpu)}%`) + temp);
    setTile("vram", l.vramUsed != null && l.vramTotal ? (l.vramUsed / l.vramTotal) * 100 : null, l.vramUsed != null ? `${gb(l.vramUsed)}${l.vramTotal ? ` <small>/ ${gb(l.vramTotal)} ГБ</small>` : " ГБ"}` : "—");
    setTile("ram", l.ramTotal ? (l.ramUsed / l.ramTotal) * 100 : null, `${gb(l.ramUsed)} <small>/ ${gb(l.ramTotal)} ГБ</small>`);
  }

  // ---------- раз в секунду, пока оверлей открыт ----------
  let open = false;
  let timer = null;
  async function tick() {
    if (!open) return;
    await Promise.all([refreshMixer(), refreshLoad()]);
    if (open) timer = setTimeout(tick, 1000);
  }
  function sync() {
    const now = document.documentElement.classList.contains("overlay-open");
    if (now === open) return;
    open = now;
    clearTimeout(timer);
    if (open) tick();
  }
  new MutationObserver(sync).observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
  sync();
})();
