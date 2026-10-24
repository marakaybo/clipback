// Слой «идёт сохранение» поверх видео: название действия, проценты, полоса, «Отмена».
// В конце — короткое «Готово» с «Показать в папке». Быстрые действия (меньше четверти
// секунды) слой не показывают вовсе — хватает обычного уведомления.

(function () {
  // ---------- чистые функции (проверяются в tests/work.test.js) ----------
  const Work = {
    /// Проценты для показа: 100 — только когда действительно готово.
    percent(f, finished = false) {
      if (finished) return 100;
      const p = Math.floor(Math.min(Math.max(+f || 0, 0), 1) * 100);
      return Math.min(p, 99);
    },
    /// «Сохраняю отрезок… 42 %».
    text(action, f) {
      return `${action} ${Work.percent(f)} %`;
    },
    /// Ход работы с ядра идёт вперёд; заметный откат — новая попытка (сжатие под размер).
    next(prev, f) {
      if (!(f >= 0)) return { f: prev, retry: false };
      if (f + 0.3 < prev) return { f, retry: true };
      return { f: Math.max(prev, f), retry: false };
    },
  };

  /// Управление слоем. `el` — корень слоя (#work), `h` — { api, reveal(path) }.
  function create(el, h) {
    const q = (s) => el.querySelector(s);
    const name = q(".work-name"), pct = q(".work-pct"), fill = q(".progress-fill"), sub = q(".work-sub");
    const cancel = q(".work-cancel"), reveal = q(".work-reveal"), close = q(".work-close");
    let job = null;
    let showT = 0, hideT = 0;

    function render() {
      if (!job) return;
      name.textContent = job.finished ? "Готово" : job.action;
      pct.textContent = job.finished ? "" : `${Work.percent(job.f)} %`;
      fill.style.width = `${Work.percent(job.f, job.finished)}%`;
      sub.textContent = job.sub || "";
      sub.hidden = !job.sub;
      cancel.hidden = job.finished || !job.cancel;
      reveal.hidden = !job.finished || !job.path;
      close.hidden = !job.finished;
      el.classList.toggle("done", !!job.finished);
    }

    function show() {
      el.hidden = false;
      document.getElementById("app").classList.add("working");
    }

    function hide() {
      clearTimeout(showT);
      clearTimeout(hideT);
      el.hidden = true;
      document.getElementById("app").classList.remove("working");
      job = null;
    }

    cancel.onclick = () => {
      if (!job || job.cancelling) return;
      job.cancelling = true;
      cancel.disabled = true;
      cancel.textContent = "Отменяю…";
      h.api.call("cancel_export").catch(() => {});
    };
    reveal.onclick = () => {
      if (job && job.path) h.reveal(job.path);
      hide();
    };
    close.onclick = hide;

    return {
      /// Начать: `action` — «Сохраняю отрезок…». Слой появится, если работа затянется.
      start(action, { canCancel = true, sub = "" } = {}) {
        hide();
        job = { action, f: 0, cancel: canCancel, sub, shown: false };
        cancel.disabled = false;
        cancel.textContent = "Отмена";
        document.getElementById("app").classList.add("busy-work");
        render();
        const j = job;
        showT = setTimeout(() => {
          if (job !== j) return;
          j.shown = true;
          show();
        }, 250);
      },
      progress(f) {
        if (!job || job.finished) return;
        const n = Work.next(job.f, f);
        job.f = n.f;
        if (n.retry) job.sub = "Файл вышел больше лимита — сжимаю сильнее";
        render();
      },
      setSub(text) {
        if (!job) return;
        job.sub = text;
        render();
      },
      /// Готово. Если слой так и не показался (быстро), вернёт false — хватит уведомления.
      done(subText, path) {
        document.getElementById("app").classList.remove("busy-work");
        if (!job || !job.shown) return hide(), false;
        Object.assign(job, { finished: true, sub: subText, path });
        render();
        hideT = setTimeout(hide, 4500);
        return true;
      },
      /// Ошибка или отмена — слой просто убирается (о причине скажет уведомление).
      fail() {
        document.getElementById("app").classList.remove("busy-work");
        hide();
      },
      active: () => !!job && !job.finished,
    };
  }

  if (typeof module !== "undefined" && module.exports) module.exports = { Work };
  else window.Work = Object.assign(Work, { create });
})();
