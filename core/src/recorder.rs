//! Рекордер: поток захвата + кодирования, кольцевой буфер и сохранение клипов.
//! Наружу — только команды (`Command`) и события (`Event`), чтобы потом
//! подключить интерфейс (Tauri) без изменений в ядре.

use crate::capture::{texture_desc, Duplicator, Frame};
use crate::convert::{create_frame_texture, Converter};
use crate::encoder::{Encoder, EncoderSettings};
use crate::gpu::{self, Gpu};
use crate::mixer::{self, AudioConfig};
use crate::ring::Ring;
use std::sync::Arc;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, ID3D11VideoProcessorOutputView};
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

#[derive(Clone, Debug)]
pub struct Config {
    pub monitor: usize,
    pub fps: u32,
    /// Мбит/с
    pub bitrate_mbps: u32,
    pub buffer_secs: u32,
    pub out_dir: PathBuf,
    /// Уменьшить картинку, если она выше этого значения (например, 1080).
    pub max_height: Option<u32>,
    pub audio: AudioConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            monitor: 0,
            fps: 60,
            bitrate_mbps: 20,
            buffer_secs: 300,
            out_dir: crate::util::videos_dir().join("Clipback"),
            max_height: None,
            audio: AudioConfig::default(),
        }
    }
}

pub enum Command {
    /// Сохранить последние `seconds` секунд (None — весь буфер).
    /// `label` — начало имени файла (обычно название игры).
    Save { seconds: Option<u32>, label: Option<String> },
    SetBufferLength(u32),
    /// PNG текущего кадра экрана.
    Screenshot { reply: Sender<Result<Vec<u8>, String>> },
    Stop,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    #[serde(rename_all = "camelCase")]
    Started { encoder: String, gpu: String, monitor: String, width: u32, height: u32, fps: u32, audio: Vec<String>, audio_error: Option<String> },
    #[serde(rename_all = "camelCase")]
    Status { buffered_secs: f64, buffer_bytes: usize, fps: f64, dropped: u64 },
    #[serde(rename_all = "camelCase")]
    Saved { path: PathBuf, seconds: f64, bytes: u64 },
    SaveFailed { message: String },
    Error { message: String },
}

pub struct Recorder {
    tx: Sender<Command>,
    pub events: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Запустить захват. Возвращается, когда всё готово (или с ошибкой).
    pub fn start(cfg: Config) -> Result<(Self, Event)> {
        let (tx, rx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("clipback-capture".into())
            .spawn(move || {
                let etx2 = etx.clone();
                if let Err(e) = run(cfg, rx, etx) {
                    let _ = etx2.send(Event::Error { message: format!("{e:#}") });
                }
            })?;
        let first = erx.recv().context("поток захвата завершился")?;
        if let Event::Error { message } = &first {
            anyhow::bail!("{message}");
        }
        Ok((Self { tx, events: erx, thread: Some(thread) }, first))
    }

    pub fn sender(&self) -> Sender<Command> {
        self.tx.clone()
    }

    pub fn save(&self, seconds: Option<u32>, label: Option<String>) {
        let _ = self.tx.send(Command::Save { seconds, label });
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Всё, что зависит от размера захватываемой картинки.
struct Pipeline {
    frame: ID3D11Texture2D,
    conv: Converter,
    pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
}

const POOL: usize = 8;

fn build_pipeline(g: &Gpu, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Pipeline> {
    let frame = create_frame_texture(&g.device, in_w, in_h)?;
    let conv = Converter::new(&g.device, &g.context, &frame, out_w, out_h, fps)?;
    let pool = (0..POOL).map(|_| conv.make_target(&g.device)).collect::<Result<_>>()?;
    Ok(Pipeline { frame, conv, pool })
}

fn output_size(w: u32, h: u32, max_h: Option<u32>) -> (u32, u32) {
    let (w, h) = match max_h {
        Some(m) if h > m => ((w as u64 * m as u64 / h as u64) as u32, m),
        _ => (w, h),
    };
    (w & !1, h & !1) // кодировщику нужны чётные размеры
}

fn run(cfg: Config, rx: Receiver<Command>, events: Sender<Event>) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        // точные таймеры Windows (по умолчанию шаг ~15 мс)
        timeBeginPeriod(1);
    }
    let r = capture_loop(&cfg, &rx, &events);
    unsafe {
        timeEndPeriod(1);
        let _ = MFShutdown();
        CoUninitialize();
    }
    r
}

fn capture_loop(cfg: &Config, rx: &Receiver<Command>, events: &Sender<Event>) -> Result<()> {
    let g = gpu::open(cfg.monitor)?;
    let mut dup = Duplicator::new(&g.device, &g.output)?;
    let (in_w, in_h) = dup.size().unwrap_or((g.width, g.height));
    let (out_w, out_h) = output_size(in_w, in_h, cfg.max_height);
    let fps = cfg.fps.clamp(10, 240);

    let mut pipe = build_pipeline(&g, in_w, in_h, out_w, out_h, fps)?;
    let mut enc = Encoder::new(
        &g.device,
        &EncoderSettings { width: out_w, height: out_h, fps, bitrate: cfg.bitrate_mbps.clamp(1, 200) * 1_000_000 },
    )?;
    let mut ring = Ring::new(cfg.buffer_secs);

    // видео и звук считают время от одной точки: start (Instant) и t0 (QPC) — один момент
    let start = Instant::now();
    let t0 = crate::audio::qpc_now();
    let (atx, arx) = mpsc::channel();
    let (hub, audio_error) = match mixer::start(&cfg.audio, t0, atx) {
        Ok(h) => (h, None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let _hub = &hub;
    let tracks = hub.as_ref().map(|h| h.tracks.clone()).unwrap_or_default();
    let track_list = tracks.clone();
    ring.set_audio_tracks(tracks.len());
    // кадр тишины для пропусков в звуке
    let silence: Arc<Vec<u8>> = Arc::new(if tracks.is_empty() { Vec::new() } else { crate::aac::silent_frame().unwrap_or_default() });

    let _ = events.send(Event::Started {
        encoder: enc.name.clone(),
        gpu: g.adapter_name.clone(),
        monitor: g.monitor_name.clone(),
        width: out_w,
        height: out_h,
        fps,
        audio: tracks,
        audio_error,
    });

    let frame_ns = 1_000_000_000u64 / fps as u64;
    let mut idx: i64 = 0;
    let mut have_frame = false;
    let mut pool_i = 0;
    let (mut encoded, mut dropped) = (0u64, 0u64);
    let mut last_status = Instant::now();
    let mut encoded_at_status = 0u64;
    // CLIPBACK_DEBUG=1 — каждые 2 с писать в журнал, на что уходит время захвата
    let debug = std::env::var_os("CLIPBACK_DEBUG").is_some();
    let mut dbg = Dbg::default();
    // кадр экрана, который держим до момента выдачи, и было ли уже копирование в этом интервале
    let mut held: Option<ID3D11Texture2D> = None;
    let mut copied = false;

    loop {
        // команды
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::Stop => return Ok(()),
                Command::SetBufferLength(s) => ring.set_length(s),
                Command::Screenshot { reply } => {
                    let r = if have_frame {
                        crate::shot::grab_png(&g, &pipe.frame).map_err(|e| format!("{e:#}"))
                    } else {
                        Err("экран ещё не захвачен".into())
                    };
                    let _ = reply.send(r);
                }
                Command::Save { seconds, label } => {
                    let snap = ring.snapshot_all(seconds);
                    let silence = silence.clone();
                    let names = track_list.clone();
                    let base = format!("{} {}", label.as_deref().unwrap_or("Clipback"), crate::util::file_stamp());
                    let path = crate::util::unique_path(&cfg.out_dir, &base, "mp4");
                    let frame_secs = 1.0 / fps as f64;
                    let ev = events.clone();
                    // запись идёт в отдельном потоке — захват не прерывается
                    std::thread::spawn(move || {
                        let packets = &snap.video;
                        let inputs: Vec<crate::mux::AudioInput> =
                            snap.audio.iter().map(|a| crate::mux::AudioInput { packets: a, silence: &silence }).collect();
                        let secs = match (packets.first(), packets.last()) {
                            (Some(a), Some(b)) => (b.pts - a.pts) as f64 / 1e7 + frame_secs,
                            _ => 0.0,
                        };
                        let r = if silence.is_empty() && !inputs.is_empty() {
                            // кадра тишины нет (не должно случаться) — сохраняем хотя бы видео
                            crate::mux::write_mp4(&path, packets, out_w, out_h)
                        } else {
                            crate::mux::write_av_named(&path, packets, &inputs, &names, out_w, out_h)
                        };
                        let _ = match r {
                            Ok(()) => {
                                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                                ev.send(Event::Saved { path, seconds: secs, bytes })
                            }
                            Err(e) => ev.send(Event::SaveFailed { message: format!("{e:#}") }),
                        };
                    });
                }
            }
        }

        // готовые пакеты от кодировщиков — в буфер
        enc.poll(&mut |p| ring.push(p))?;
        while let Ok((k, p)) = arx.try_recv() {
            ring.push_audio(k, p);
        }

        let due = start + Duration::from_nanos(idx as u64 * frame_ns);
        let now = Instant::now();
        // Игра может рисовать сотни кадров в секунду, а видео нужно 60. Копировать каждый —
        // лишняя работа видеокарте (под нагрузкой из-за неё и терялись кадры). Поэтому:
        // кадр из первой половины интервала копируем не больше одного раза,
        // кадр из второй половины держим и копируем ровно в момент выдачи.
        if held.is_none() && now + Duration::from_micros(800) < due {
            let wait_ms = (((due - now).as_micros() + 999) / 1000).clamp(1, 100) as u32;
            let t_acq = Instant::now();
            let got = dup.acquire(wait_ms)?;
            dbg.acquire_ms += t_acq.elapsed().as_secs_f64() * 1000.0;
            match got {
                Frame::New(tex) => {
                    dbg.new_frames += 1;
                    let d = texture_desc(&tex);
                    let fd = texture_desc(&pipe.frame);
                    if (d.Width, d.Height) != (fd.Width, fd.Height) {
                        // разрешение экрана изменилось — картинка масштабируется в прежний размер видео
                        dup.release();
                        pipe = build_pipeline(&g, d.Width, d.Height, out_w, out_h, fps)?;
                        continue;
                    }
                    let left = due.saturating_duration_since(Instant::now());
                    if left > Duration::from_nanos(frame_ns / 2) {
                        if !copied {
                            unsafe { g.context.CopyResource(&pipe.frame, &tex) };
                            copied = true;
                            have_frame = true;
                            dbg.copies += 1;
                        }
                        dup.release();
                    } else {
                        held = Some(tex);
                    }
                }
                Frame::Timeout => dbg.timeouts += 1,
                Frame::Lost => dbg.lost += 1,
            }
            continue;
        }
        if now < due {
            // кадр уже взят (или осталась доля миллисекунды) — просто дожидаемся момента выдачи
            std::thread::sleep(due - now);
            continue;
        }
        if let Some(tex) = held.take() {
            unsafe { g.context.CopyResource(&pipe.frame, &tex) };
            dup.release();
            have_frame = true;
            dbg.copies += 1;
        }
        copied = false;

        // пора выдать очередной кадр (если экран не менялся — повторяем последний).
        // Запрос кадра от кодировщика мог прийти, пока ждали экран, — проверяем ещё раз
        // и даём ему до трети кадра, прежде чем считать кадр пропущенным.
        if have_frame && !enc.wants_input() {
            let deadline = Instant::now() + Duration::from_nanos(frame_ns / 3);
            loop {
                enc.poll(&mut |p| ring.push(p))?;
                if enc.wants_input() || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_micros(300));
            }
        }
        if have_frame && enc.wants_input() {
            let (tex, view) = &pipe.pool[pool_i];
            pipe.conv.convert(view)?;
            enc.push(tex, idx)?;
            pool_i = (pool_i + 1) % POOL;
            encoded += 1;
        } else if have_frame {
            dropped += 1;
            dbg.enc_busy += 1;
        }
        let late = Instant::now().saturating_duration_since(due).as_secs_f64() * 1000.0;
        dbg.late_ms = dbg.late_ms.max(late);
        idx += 1;

        // сильно отстали (система подвисла) — пропускаем, а не догоняем рывком
        let should = (start.elapsed().as_nanos() / frame_ns as u128) as i64;
        if should - idx > fps as i64 / 2 {
            dbg.skipped += (should - idx) as u64;
            dropped += (should - idx) as u64;
            idx = should;
        }

        if last_status.elapsed() >= Duration::from_secs(2) {
            let secs = last_status.elapsed().as_secs_f64();
            let _ = events.send(Event::Status {
                buffered_secs: ring.duration(),
                buffer_bytes: ring.bytes(),
                fps: (encoded - encoded_at_status) as f64 / secs,
                dropped,
            });
            if debug {
                crate::util::log(format!("отладка захвата: {dbg:?}"));
                dbg = Dbg::default();
            }
            encoded_at_status = encoded;
            last_status = Instant::now();
        }
    }
}

#[derive(Debug, Default)]
struct Dbg {
    new_frames: u32,
    copies: u32,
    timeouts: u32,
    lost: u32,
    enc_busy: u32,
    skipped: u64,
    acquire_ms: f64,
    late_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_is_even_and_scaled() {
        assert_eq!(output_size(2560, 1440, None), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(1080)), (1920, 1080));
        assert_eq!(output_size(1366, 768, None), (1366, 768));
        assert_eq!(output_size(1365, 767, None), (1364, 766));
        assert_eq!(output_size(1920, 1080, Some(1440)), (1920, 1080));
    }
}
