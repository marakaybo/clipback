// Звук плеера по дорожкам. <video> играет только первую дорожку файла («Всё вместе»),
// поэтому выключенная дорожка или её громкость в плеере не слышны. Пока ничего не меняли,
// играет само видео. Когда меняли — видео играет без звука, а дорожки (отдельные .m4a,
// export::preview_tracks) идут через Web Audio, каждая со своей громкостью, вслед за видео.

(function () {
  const MIX = "Всё вместе";
  const SYS = "Звук системы";
  const MIC = "Микрофон";
  // как в core/src/voicemix.rs: голос чуть громче игры, пределы усиления
  const VOICE_FLOOR = 0.01, GAME_FLOOR = 0.003, OVER = 1.12;
  const MIN_BOOST = 0.5, MAX_BOOST = 6, MAX_SYS = 2.8;
  /// Расхождение звука с видео, после которого звук переставляется (секунды).
  const DRIFT = 0.04;

  const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
  const mean = (v) => (v.length ? v.reduce((a, x) => a + x, 0) / v.length : 0);

  // ---------- чистые функции (проверяются в tests/trackaudio.test.js) ----------
  const TrackAudio = {
    /// Примерно как выравнивание «Всё вместе» в ядре, по пикам дорожек (форма волны):
    /// множители системы и микрофона.
    level(sysPeaks, micPeaks) {
      const voice = mean((micPeaks || []).filter((x) => x > VOICE_FLOOR));
      const game = mean((sysPeaks || []).filter((x) => x > GAME_FLOOR));
      if (!voice || !game) return { sys: 1, mic: 1 };
      const c = (game * OVER) / voice;
      const mic = clamp(c, MIN_BOOST, MAX_BOOST);
      return { sys: clamp(mic / c, 1, MAX_SYS), mic };
    },

    /// Что играть: { native: true } — само видео; иначе громкость каждой дорожки файла.
    /// `on(i)`, `vol(i)` — переключатель и громкость (%) дорожки, `lv` — из level().
    plan(names, on, vol, lv) {
      const sys = names.indexOf(SYS), mic = names.indexOf(MIC);
      const mix = names[0] === MIX && sys >= 0 && mic >= 0;
      const tracks = [];
      names.forEach((_, i) => {
        if (mix && i === 0) return;
        let gain = on(i) ? vol(i) / 100 : 0;
        // звук приложений уже есть в «Звуке системы» — дважды не играем
        if (i !== sys && i !== mic && sys >= 0 && on(sys)) gain = 0;
        tracks.push({ index: i, gain });
      });
      const both = mix && on(sys) && on(mic);
      if (both && lv) {
        for (const t of tracks) {
          if (t.index === sys) t.gain *= lv.sys;
          if (t.index === mic) t.gain *= lv.mic;
        }
      }
      // ничего не меняли — первая дорожка файла и так звучит как надо
      const native = mix
        ? both && vol(sys) === 100 && vol(mic) === 100
        : tracks.every((t) => (t.index === 0 ? t.gain === 1 : t.gain === 0));
      return { native, tracks };
    },

    /// Переставить звук: разошёлся с видео больше чем на DRIFT.
    needsSync(videoT, audioT, latency = 0) {
      return Math.abs(audioT - (videoT + latency)) > DRIFT;
    },
  };

  /// Плеер дорожек при `video`. h — { api, fileUrl }.
  function create(video, h) {
    let ctx = null, master = null;
    let els = []; // [{ a: <audio>, g: GainNode, index }]
    let clipKey = "", token = 0;
    let active = false, userMuted = video.muted;
    let pending = "", latest = null, buffering = false;
    const moving = () => !video.paused && !video.ended && !video.seeking && !buffering && video.readyState >= 3;

    const latency = () => (ctx ? (ctx.outputLatency || 0) + (ctx.baseLatency || 0) : 0);
    const muteVideo = () => (video.muted = userMuted || active);
    const out = () => master && (master.gain.value = userMuted ? 0 : video.volume);

    function ensureCtx() {
      if (ctx) return;
      ctx = new AudioContext();
      master = ctx.createGain();
      // сумма дорожек не должна хрипеть: мягкий ограничитель, как в ядре
      const lim = ctx.createDynamicsCompressor();
      lim.threshold.value = -1;
      lim.knee.value = 0;
      lim.ratio.value = 20;
      lim.attack.value = 0.002;
      lim.release.value = 0.15;
      master.connect(lim).connect(ctx.destination);
      out();
    }

    function drop() {
      for (const e of els) {
        e.a.pause();
        e.a.removeAttribute("src");
        e.a.load();
        e.g.disconnect();
      }
      els = [];
      clipKey = "";
    }

    function sync(force) {
      if (!active) return;
      const t = video.currentTime, lat = moving() ? latency() * video.playbackRate : 0;
      for (const e of els) {
        if (e.a.playbackRate !== video.playbackRate) e.a.playbackRate = video.playbackRate;
        if (e.a.seeking || e.a.readyState < 1) continue;
        if (force || TrackAudio.needsSync(t, e.a.currentTime, lat)) e.a.currentTime = Math.min(t + lat, Number.isFinite(e.a.duration) ? e.a.duration : Infinity);
      }
    }

    function play() {
      if (!active || !moving() || !els.length) return;
      if (ctx.state === "suspended") ctx.resume().catch(() => {});
      sync(false);
      for (const e of els) if (e.a.paused && e.a.readyState >= 1) e.a.play().catch(() => {});
    }
    function pause() {
      for (const e of els) e.a.pause();
    }

    function stop() {
      if (!active) return;
      active = false;
      pause();
      muteVideo();
    }

    // звук идёт за видео только по событиям — на паузе и в фоне ничего не работает
    video.addEventListener("playing", () => { buffering = false; play(); });
    video.addEventListener("seeking", pause);
    video.addEventListener("ended", pause);
    video.addEventListener("pause", pause);
    video.addEventListener("waiting", () => { buffering = true; pause(); });
    video.addEventListener("seeked", () => (video.paused ? sync(true) : play()));
    video.addEventListener("ratechange", () => sync(false));
    video.addEventListener("timeupdate", () => moving() && sync(false));
    video.addEventListener("volumechange", out);

    function gains(tracks) {
      for (const e of els) {
        const t = tracks.find((x) => x.index === e.index);
        e.g.gain.value = t ? t.gain : 0;
      }
    }

    return {
      /// Клип и что играть (TrackAudio.plan). null или native — играет само видео.
      async set(clip, p) {
        const key = clip ? `${clip.path}|${clip.modified}` : "";
        latest = p;
        if (!clip || !p || p.native) {
          token++;
          pending = "";
          h.onLoading?.(false);
          stop();
          if (key !== clipKey) drop();
          return;
        }
        if (key === pending) return;
        if (key === clipKey && els.length) {
          gains(p.tracks);
          if (!active) { active = true; muteVideo(); play(); }
          return;
        }
        const my = ++token;
        pending = key;
        h.onLoading?.(true);
        drop();
        active = true;
        muteVideo();
        const fail = (error) => {
          if (my !== token) return;
          token++;
          drop();
          pending = "";
          h.onLoading?.(false);
          h.onError?.(String(error));
        };
        try {
          const indices = p.tracks.map((t) => t.index);
          const files = await h.api.call("preview_tracks", { path: clip.path, tracks: indices });
          if (my !== token) return;
          if (files.length !== indices.length) throw Error("Не все дорожки доступны");
          ensureCtx();
          files.forEach((f, k) => {
            const a = new Audio();
            a.crossOrigin = "anonymous";
            a.preload = "auto";
            const g = ctx.createGain();
            ctx.createMediaElementSource(a).connect(g).connect(master);
            els.push({ a, g, index: indices[k] });
            // Each decoder can initially buffer, or stall after a seek. Align it
            // to the current video position when it can actually play again.
            a.addEventListener("canplay", () => {
              if (els.some((entry) => entry.a === a)) play();
            });
            a.addEventListener("error", () => {
              if (els.some((entry) => entry.a === a)) fail("Не удалось прочитать дорожку");
            });
            a.src = h.fileUrl(f);
          });
          clipKey = key;
          pending = "";
          h.onLoading?.(false);
          gains(latest.tracks);
          out();
          if (moving()) play();
          else sync(true);
        } catch (error) {
          fail(error);
        }
      },
      muted: () => userMuted,
      setMuted(m) {
        userMuted = m;
        muteVideo();
        out();
      },
      active: () => active,
      latency,
    };
  }

  if (typeof module !== "undefined" && module.exports) module.exports = { TrackAudio, create };
  else window.TrackAudio = Object.assign(TrackAudio, { create });
})();
