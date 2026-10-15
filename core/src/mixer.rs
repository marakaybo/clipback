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
use std::sync::Arc;
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
    /// Отдельные дорожки приложений (расширенный режим).
    pub apps: Vec<AppTrack>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { system: true, mic: false, mic_device: None, mic_gain: 1.0, apps: Vec::new() }
    }
}

/// Источник и его громкость.
pub struct SourceDef {
    pub kind: SourceKind,
    pub gain: f32,
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
        sources.push(SourceDef { kind: SourceKind::System, gain: 1.0 });
        sources.len() - 1
    });
    let mic = cfg.mic.then(|| {
        sources.push(SourceDef { kind: SourceKind::Mic(cfg.mic_device.clone()), gain: cfg.mic_gain.clamp(0.1, 8.0) });
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
        sources.push(SourceDef { kind: SourceKind::App(a.exe.clone()), gain: 1.0 });
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

fn mixer_loop(lay: Layout, mut encoders: Vec<AacEncoder>, t0: i64, out: Sender<(usize, AudioPacket)>, stop: Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel::<Chunk>();
    let handles: Vec<_> = lay
        .sources
        .iter()
        .enumerate()
        .map(|(i, s)| audio::spawn_source(i, s.kind.clone(), t0, tx.clone(), stop.clone()))
        .collect();
    drop(tx);

    let mut st = MixState::new(lay.sources.len(), Instant::now());
    let mut packets = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(c) => {
                st.push(c, Instant::now());
                while let Ok(c) = rx.try_recv() {
                    st.push(c, Instant::now());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Some((pos, parts)) = st.next_block(Instant::now()) {
            for (k, (t, enc)) in lay.tracks.iter().zip(encoders.iter_mut()).enumerate() {
                let s = render_track(t, &lay.sources, &parts);
                packets.clear();
                if let Err(e) = enc.encode(pos, &s, &mut packets) {
                    crate::util::log(format!("AAC: {e:#}"));
                }
                for p in packets.drain(..) {
                    let _ = out.send((k, p));
                }
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
}

#[cfg(test)]
#[path = "tests/mixer.rs"]
mod tests;
