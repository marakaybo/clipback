//! Звуковой поток записи: запускает источники, сводит их и кодирует дорожки AAC.
//!
//! Обычный режим: «Звук системы» и «Микрофон». Если пишутся оба, первой в файле идёт
//! дорожка «Всё вместе» — её играют плееры и Discord, поэтому там полный звук.
//! Расширенный режим: плюс отдельная дорожка на каждое выбранное приложение.

use crate::aac::{AacEncoder, AudioPacket};
use crate::audio::{self, Chunk, SourceKind, RATE};
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct AppTrack {
    /// Как называть дорожку.
    pub name: String,
    /// Имя exe приложения, например Discord.exe.
    pub exe: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioConfig {
    pub system: bool,
    pub mic: bool,
    /// None — микрофон по умолчанию.
    pub mic_device: Option<String>,
    /// Усиление микрофона (1.0 — как есть).
    pub mic_gain: f32,
    /// Автоусиление микрофона: тихий голос поднимается до нормальной громкости.
    pub mic_agc: bool,
    /// Отдельные дорожки приложений (расширенный режим).
    pub apps: Vec<AppTrack>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { system: true, mic: false, mic_device: None, mic_gain: 1.0, mic_agc: true, apps: Vec::new() }
    }
}

/// Источник и его громкость.
pub struct SourceDef {
    pub kind: SourceKind,
    pub gain: f32,
    pub agc: bool,
}

/// Автоусиление голоса. Громкость микрофона в Windows часто занижена (её крутят Discord и
/// фирменные программы микрофонов), и голос в записи едва слышен. Здесь уровень голоса
/// подтягивается к нормальному, а тишина между фразами не раздувается в шум.
pub struct Agc {
    gain: f32,
    env: f32,
}

impl Agc {
    /// Куда тянем пики голоса.
    const TARGET: f32 = 0.45;
    /// Больше чем в 50 раз (+34 дБ) не усиливаем.
    const MAX_GAIN: f32 = 50.0;
    /// Тише этого — не голос, а шум: усиление не растёт.
    const FLOOR: f32 = 0.0015;

    pub fn new() -> Self {
        Self { gain: 1.0, env: 0.0 }
    }

    /// Обработать стерео-сэмплы на месте.
    pub fn process(&mut self, samples: &mut [f32]) {
        let rate = (RATE * 2) as f32; // стерео
        let env_release = (-1.0 / (0.25 * rate)).exp(); // огибающая гаснет за ~0,25 с
        let up = 1.0 / (1.2 * rate); // усиление растёт плавно (~1,2 с)
        let down = 1.0 / (0.01 * rate); // и быстро падает на громком (~10 мс)
        for x in samples.iter_mut() {
            let a = x.abs();
            self.env = if a > self.env { a } else { self.env * env_release };
            if self.env > Self::FLOOR {
                let want = (Self::TARGET / self.env).clamp(1.0, Self::MAX_GAIN);
                let k = if want < self.gain { down } else { up };
                self.gain += (want - self.gain) * k.min(1.0);
            }
            *x = soft_clip(*x * self.gain);
        }
    }
}

impl Default for Agc {
    fn default() -> Self {
        Self::new()
    }
}

/// Дорожка файла: название и какие источники в неё сводятся.
pub struct TrackDef {
    pub name: String,
    pub sources: Vec<usize>,
}

pub struct Layout {
    pub sources: Vec<SourceDef>,
    pub tracks: Vec<TrackDef>,
}

pub fn layout(cfg: &AudioConfig) -> Layout {
    let mut sources = Vec::new();
    let mut tracks = Vec::new();
    let sys = cfg.system.then(|| {
        sources.push(SourceDef { kind: SourceKind::System, gain: 1.0, agc: false });
        sources.len() - 1
    });
    let mic = cfg.mic.then(|| {
        sources.push(SourceDef { kind: SourceKind::Mic(cfg.mic_device.clone()), gain: cfg.mic_gain.clamp(0.1, 8.0), agc: cfg.mic_agc });
        sources.len() - 1
    });
    if let (Some(s), Some(m)) = (sys, mic) {
        tracks.push(TrackDef { name: "Всё вместе".into(), sources: vec![s, m] });
    }
    if let Some(s) = sys {
        tracks.push(TrackDef { name: "Звук системы".into(), sources: vec![s] });
    }
    if let Some(m) = mic {
        tracks.push(TrackDef { name: "Микрофон".into(), sources: vec![m] });
    }
    for a in &cfg.apps {
        if a.exe.trim().is_empty() {
            continue;
        }
        sources.push(SourceDef { kind: SourceKind::App(a.exe.clone()), gain: 1.0, agc: false });
        let name = if a.name.trim().is_empty() { a.exe.clone() } else { a.name.trim().to_string() };
        tracks.push(TrackDef { name, sources: vec![sources.len() - 1] });
    }
    Layout { sources, tracks }
}

pub fn track_names(cfg: &AudioConfig) -> Vec<String> {
    layout(cfg).tracks.into_iter().map(|t| t.name).collect()
}

pub struct AudioHub {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub tracks: Vec<String>,
}

impl Drop for AudioHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Запустить звук. Пакеты уходят в `out` как (номер дорожки, кадр AAC).
pub fn start(cfg: &AudioConfig, t0: i64, out: Sender<(usize, AudioPacket)>) -> Result<Option<AudioHub>> {
    let lay = layout(cfg);
    if lay.tracks.is_empty() {
        return Ok(None);
    }
    // пробный кодировщик: если AAC в системе нет, узнаем сразу, а не в потоке
    drop(AacEncoder::new()?);
    let names: Vec<String> = lay.tracks.iter().map(|t| t.name.clone()).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new().name("clipback-mixer".into()).spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        // объекты Media Foundation живут в своём потоке — создаём их здесь
        match (0..lay.tracks.len()).map(|_| AacEncoder::new()).collect::<Result<Vec<_>>>() {
            Ok(encoders) => mixer_loop(lay, encoders, t0, out, stop2),
            Err(e) => crate::util::log(format!("AAC: {e:#}")),
        }
    })?;
    Ok(Some(AudioHub { stop, thread: Some(thread), tracks: names }))
}

struct Queue {
    base: Option<i64>,
    buf: VecDeque<f32>,
}

impl Queue {
    fn new() -> Self {
        Self { base: None, buf: VecDeque::new() }
    }

    fn end(&self) -> Option<i64> {
        self.base.map(|b| b + self.buf.len() as i64 / 2)
    }

    fn push(&mut self, c: Chunk) {
        let Some(end) = self.end() else {
            self.base = Some(c.pos);
            self.buf.extend(c.samples);
            return;
        };
        let d = c.pos - end;
        if d > 0 {
            self.buf.extend(std::iter::repeat(0.0).take(d as usize * 2));
            self.buf.extend(c.samples);
        } else {
            let skip = ((-d) as usize * 2).min(c.samples.len());
            self.buf.extend(c.samples.into_iter().skip(skip));
        }
    }

    /// Взять `n` кадров начиная с позиции `from` (чего нет — тишина).
    fn take(&mut self, from: i64, n: usize) -> Vec<f32> {
        let base = self.base.unwrap_or(from);
        if base < from {
            let drop = (((from - base) as usize) * 2).min(self.buf.len());
            self.buf.drain(..drop);
        }
        let lead = if base > from { ((base - from) as usize).min(n) } else { 0 };
        let mut out = vec![0.0f32; lead * 2];
        let have = (n - lead) * 2;
        let k = have.min(self.buf.len());
        out.extend(self.buf.drain(..k));
        out.resize(n * 2, 0.0);
        self.base = Some(from + n as i64);
        out
    }
}

/// Источник считаем пропавшим, если от него ничего не приходило столько времени.
/// Важно: отставание по позиции — не повод выбрасывать звук, его надо дождаться.
const SILENT_AFTER: Duration = Duration::from_millis(600);

/// Сведение: копит звук источников и выдаёт блоки, которые покрыли все живые источники.
pub(crate) struct MixState {
    queues: Vec<Queue>,
    last_rx: Vec<Option<Instant>>,
    enc_pos: Option<i64>,
    started: Instant,
}

impl MixState {
    pub(crate) fn new(n: usize, now: Instant) -> Self {
        Self { queues: (0..n).map(|_| Queue::new()).collect(), last_rx: vec![None; n], enc_pos: None, started: now }
    }

    pub(crate) fn push(&mut self, c: Chunk, now: Instant) {
        let i = c.source;
        if i < self.queues.len() {
            self.last_rx[i] = Some(now);
            self.queues[i].push(c);
        }
    }

    fn alive(&self, i: usize, now: Instant) -> bool {
        self.last_rx[i].is_some_and(|t| now.duration_since(t) < SILENT_AFTER)
    }

    /// Следующий блок: позиция и звук каждого источника (одинаковой длины).
    pub(crate) fn next_block(&mut self, now: Instant) -> Option<(i64, Vec<Vec<f32>>)> {
        let n_src = self.queues.len();
        if self.enc_pos.is_none() {
            // начинаем, когда все источники дали звук (или через секунду — с теми, что есть)
            let bases: Vec<i64> = self.queues.iter().filter_map(|q| q.base).collect();
            if bases.len() == n_src || (!bases.is_empty() && now.duration_since(self.started) > Duration::from_secs(1)) {
                self.enc_pos = bases.iter().copied().max();
            } else {
                return None;
            }
        }
        let pos = self.enc_pos?;
        // готово до позиции, которую покрыли все живые источники
        let ready = (0..n_src).filter(|&i| self.alive(i, now)).filter_map(|i| self.queues[i].end()).min()?;
        let n = (ready - pos).min(RATE as i64 / 5);
        if n < RATE as i64 / 100 {
            return None;
        }
        let n = n as usize;
        let parts = self.queues.iter_mut().map(|q| q.take(pos, n)).collect();
        self.enc_pos = Some(pos + n as i64);
        Some((pos, parts))
    }
}

/// Мягкое ограничение: громкое сведение не хрипит, тихое не меняется.
pub fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.9 {
        x
    } else {
        x.signum() * (0.9 + 0.1 * ((a - 0.9) / 0.1).tanh())
    }
}

/// Звук дорожки из блоков источников.
fn render_track(t: &TrackDef, sources: &[SourceDef], parts: &[Vec<f32>]) -> Vec<f32> {
    let len = parts.first().map_or(0, |p| p.len());
    (0..len)
        .map(|i| {
            let v: f32 = t.sources.iter().map(|&s| parts[s][i] * sources[s].gain).sum();
            if t.sources.len() > 1 || sources[t.sources[0]].gain > 1.0 {
                soft_clip(v)
            } else {
                v
            }
        })
        .collect()
}

// ---------- что на самом деле слышит запись ----------
// Индикатор устройства в Windows показывает сигнал микрофона, но до записи может доходить
// тишина (шумоподавление, гейт, другая программа). Поэтому уровень меряем здесь — после захвата.

/// Уровень источника звука, как его получает запись.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceLevel {
    /// "system" | "mic" | "app"
    pub kind: &'static str,
    /// Пик за последние доли секунды (0…1, с учётом усиления).
    pub peak: f32,
    /// Сколько секунд подряд в запись идёт тишина — тише −70 дБ (0 — звук есть).
    pub zero_secs: f64,
    /// Сколько секунд источник пишется.
    pub running_secs: f64,
}

struct LevelState {
    kind: &'static str,
    gain: f32,
    peak: f32,
    peak_at: Instant,
    zero_since: Option<Instant>,
    started: Instant,
}

static LEVELS: Mutex<Vec<LevelState>> = Mutex::new(Vec::new());

/// Тише этого (−70 дБ) — в клипе не слышно: только шум микрофона, а не голос.
const QUIET: f32 = 3e-4;

/// Уровни источников текущей записи (пусто, если звук не пишется).
pub fn levels() -> Vec<SourceLevel> {
    let now = Instant::now();
    let l = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    l.iter()
        .map(|s| {
            // пик плавно гаснет, как у обычного индикатора
            let age = now.duration_since(s.peak_at).as_secs_f32();
            SourceLevel {
                kind: s.kind,
                peak: s.peak * (-age * 6.0).exp(),
                zero_secs: s.zero_since.map_or(0.0, |t| now.duration_since(t).as_secs_f64()),
                running_secs: now.duration_since(s.started).as_secs_f64(),
            }
        })
        .collect()
}

fn note_level(i: usize, samples: &[f32], now: Instant) {
    let mut l = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = l.get_mut(i) else { return };
    let peak = samples.iter().fold(0f32, |a, &b| a.max(b.abs())) * s.gain;
    if peak > QUIET {
        s.zero_since = None;
    } else if s.zero_since.is_none() {
        s.zero_since = Some(now);
    }
    let cur = s.peak * (-now.duration_since(s.peak_at).as_secs_f32() * 6.0).exp();
    if peak >= cur {
        s.peak = peak;
        s.peak_at = now;
    }
}

fn kind_name(k: &SourceKind) -> &'static str {
    match k {
        SourceKind::System => "system",
        SourceKind::Mic(_) => "mic",
        SourceKind::App(_) => "app",
    }
}

fn mixer_loop(lay: Layout, mut encoders: Vec<AacEncoder>, t0: i64, out: Sender<(usize, AudioPacket)>, stop: Arc<AtomicBool>) {
    {
        let now = Instant::now();
        *LEVELS.lock().unwrap_or_else(|e| e.into_inner()) = lay
            .sources
            .iter()
            .map(|s| LevelState { kind: kind_name(&s.kind), gain: s.gain, peak: 0.0, peak_at: now, zero_since: Some(now), started: now })
            .collect();
    }
    let (tx, rx) = mpsc::channel::<Chunk>();
    let handles: Vec<_> = lay
        .sources
        .iter()
        .enumerate()
        .map(|(i, s)| audio::spawn_source(i, s.kind.clone(), t0, tx.clone(), stop.clone()))
        .collect();
    drop(tx);

    let mut st = MixState::new(lay.sources.len(), Instant::now());
    let mut agcs: Vec<Option<Agc>> = lay.sources.iter().map(|s| s.agc.then(Agc::new)).collect();
    let mut packets = Vec::new();
    // сводка раз в 5 минут: какой звук на самом деле ушёл в каждую дорожку файла
    let mut report_at = Instant::now();
    let mut track_peak = vec![0f32; lay.tracks.len()];
    let mut track_secs = vec![0f64; lay.tracks.len()];

    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(c) => {
                let now = Instant::now();
                // автоусиление — до замера уровня: индикатор «в записи» показывает то, что попадёт в файл
                let mut take = |mut c: Chunk| {
                    if let Some(Some(a)) = agcs.get_mut(c.source) {
                        a.process(&mut c.samples);
                    }
                    note_level(c.source, &c.samples, now);
                    st.push(c, now);
                };
                take(c);
                while let Ok(c) = rx.try_recv() {
                    take(c);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Some((pos, parts)) = st.next_block(Instant::now()) {
            for (k, (t, enc)) in lay.tracks.iter().zip(encoders.iter_mut()).enumerate() {
                let s = render_track(t, &lay.sources, &parts);
                track_peak[k] = s.iter().fold(track_peak[k], |a, &b| a.max(b.abs()));
                track_secs[k] += s.len() as f64 / 2.0 / RATE as f64;
                packets.clear();
                if let Err(e) = enc.encode(pos, &s, &mut packets) {
                    crate::util::log(format!("AAC: {e:#}"));
                }
                for p in packets.drain(..) {
                    let _ = out.send((k, p));
                }
            }
        }
        if report_at.elapsed() >= audio::Diag::every() {
            let mins = report_at.elapsed().as_secs_f64() / 60.0;
            report_at = Instant::now();
            let parts: Vec<String> = lay
                .tracks
                .iter()
                .enumerate()
                .map(|(k, t)| {
                    let p = track_peak[k];
                    let db = if p > 0.0 { format!("{:.0} дБ", 20.0 * p.log10()) } else { "тишина".into() };
                    format!("«{}» {:.0} с, пик {db}", t.name, track_secs[k])
                })
                .collect();
            crate::util::log(format!("звук в файл за {mins:.0} мин: {}", parts.join("; ")));
            track_peak.iter_mut().for_each(|p| *p = 0.0);
            track_secs.iter_mut().for_each(|s| *s = 0.0);
        }
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    LEVELS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
#[path = "tests/mixer.rs"]
mod tests;
