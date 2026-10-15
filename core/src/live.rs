//! Полноценная запись: кадры пишутся в MP4 сразу, по мере кодирования, а не из буфера.
//!
//! Запись идёт в своём потоке — диск не задерживает захват. Пока запись не закончена,
//! файл называется `.part`; в конце дописывается оглавление MP4 и файл переименовывается.

use crate::aac::AudioPacket;
use crate::encoder::Packet;
use crate::mux::{self, Writer, TIMESCALE};
use anyhow::{bail, Result};
use bytes::Bytes;
use mp4::Mp4Sample;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Запись в MP4 по одному кадру. Первый видеокадр должен быть ключевым.
pub struct LiveWriter {
    w: Writer,
    tmp: PathBuf,
    path: PathBuf,
    names: Vec<String>,
    /// Время первого кадра, 100 нс.
    t0: i64,
    /// Видеокадр ждёт следующего: длительность кадра известна только по нему.
    pending: Option<(u64, Arc<Packet>)>,
    last_dur: u64,
    /// Позиция следующего кадра AAC в каждой дорожке (отсчёты от начала записи).
    audio_pos: Vec<i64>,
    silence: Bytes,
}

impl LiveWriter {
    pub fn create(path: &Path, first: &Packet, names: &[String], silence: &[u8], width: u32, height: u32) -> Result<Self> {
        if !first.key {
            bail!("запись должна начинаться с ключевого кадра");
        }
        let Some((sps, pps)) = mux::find_sps_pps(&first.data) else { bail!("в первом кадре нет SPS/PPS") };
        let tmp = path.with_extension("part");
        let mut w = mux::new_writer(&tmp)?;
        mux::add_video(&mut w, width, height, sps, pps, TIMESCALE as u32)?;
        // без кадра тишины пропуски в звуке не заполнить — тогда пишем только видео
        let tracks = if silence.is_empty() { 0 } else { names.len() };
        for _ in 0..tracks {
            mux::add_audio(&mut w, mux::our_aac(), crate::audio::RATE)?;
        }
        Ok(Self {
            w,
            tmp,
            path: path.to_path_buf(),
            names: names[..tracks].to_vec(),
            t0: first.pts,
            pending: None,
            last_dur: TIMESCALE / 60,
            audio_pos: vec![0; tracks],
            silence: Bytes::copy_from_slice(silence),
        })
    }

    fn ts(&self, pts: i64) -> u64 {
        ((pts - self.t0).max(0) as u64 * TIMESCALE + 5_000_000) / 10_000_000
    }

    pub fn video(&mut self, p: Arc<Packet>) -> Result<()> {
        let start = self.ts(p.pts);
        if let Some((s, prev)) = self.pending.take() {
            if start <= s {
                // кадр не позже предыдущего (не должно случаться) — пропускаем
                self.pending = Some((s, prev));
                return Ok(());
            }
            self.last_dur = start - s;
            self.write_video(s, &prev, self.last_dur)?;
        }
        self.pending = Some((start, p));
        Ok(())
    }

    fn write_video(&mut self, start: u64, p: &Packet, dur: u64) -> Result<()> {
        self.w.write_sample(
            1,
            &Mp4Sample { start_time: start, duration: dur as u32, rendering_offset: 0, is_sync: p.key, bytes: Bytes::from(mux::annexb_to_avcc(&p.data)) },
        )?;
        Ok(())
    }

    /// Кадр AAC дорожки `track`. Пропуски заполняются тишиной, наложения отбрасываются.
    pub fn audio(&mut self, track: usize, p: &AudioPacket) -> Result<()> {
        let Some(&pos) = self.audio_pos.get(track) else { return Ok(()) };
        let frame = crate::aac::FRAME;
        let t0 = self.t0 * crate::audio::RATE as i64 / 10_000_000;
        let rel = p.pos - t0;
        if rel + frame <= 0 || rel - pos <= -frame {
            return Ok(());
        }
        let mut pos = pos;
        // большой пропуск (звук пропадал) — не больше минуты тишины за раз
        let mut gaps = 0;
        while rel - pos >= frame && gaps < 3000 {
            self.w.write_sample(track as u32 + 2, &mux::audio_sample(pos, self.silence.clone()))?;
            pos += frame;
            gaps += 1;
        }
        self.w.write_sample(track as u32 + 2, &mux::audio_sample(pos, Bytes::from(p.data.clone())))?;
        self.audio_pos[track] = pos + frame;
        Ok(())
    }

    /// Дописать последний кадр и оглавление. Возвращает длину записи в секундах.
    pub fn finish(mut self) -> Result<(PathBuf, f64)> {
        let mut end = 0;
        if let Some((s, p)) = self.pending.take() {
            self.write_video(s, &p, self.last_dur)?;
            end = s + self.last_dur;
        }
        // звук дотягиваем тишиной до конца видео, чтобы дорожки были одной длины
        let rate = crate::audio::RATE as u64;
        let end_samples = (end * rate / TIMESCALE) as i64;
        for k in 0..self.audio_pos.len() {
            while self.audio_pos[k] + crate::aac::FRAME <= end_samples {
                let pos = self.audio_pos[k];
                self.w.write_sample(k as u32 + 2, &mux::audio_sample(pos, self.silence.clone()))?;
                self.audio_pos[k] += crate::aac::FRAME;
            }
        }
        let LiveWriter { mut w, tmp, path, names, .. } = self;
        let r = (|| {
            w.write_end()?;
            drop(w);
            if !names.is_empty() {
                mux::write_track_names(&tmp, &names)?;
            }
            Ok(())
        })();
        mux::finish(&tmp, &path, r)?;
        Ok((path, end as f64 / TIMESCALE as f64))
    }
}

pub enum Msg {
    Video(Arc<Packet>),
    Audio(usize, Arc<AudioPacket>),
}

/// Поток записи. Канал закрыт — запись заканчивается и файл дописывается.
pub struct LiveRec {
    tx: Option<Sender<Msg>>,
    thread: Option<JoinHandle<()>>,
    pub path: PathBuf,
}

/// Чем закончилась запись: (путь, секунды, байты) или ошибка.
pub type Done = Box<dyn FnOnce(Result<(PathBuf, f64, u64)>) + Send>;

impl LiveRec {
    /// `pre` — уже закодированное начало (от последнего ключевого кадра буфера).
    pub fn start(path: PathBuf, names: Vec<String>, silence: Arc<Vec<u8>>, width: u32, height: u32, pre: (Vec<Arc<Packet>>, Vec<Vec<Arc<AudioPacket>>>), done: Done) -> Self {
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        let thread = std::thread::Builder::new()
            .name("clipback-record".into())
            .spawn(move || done(write_loop(&p, &names, &silence, width, height, pre, rx)))
            .ok();
        Self { tx: Some(tx), thread, path }
    }

    pub fn send(&self, m: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(m);
        }
    }

    /// Закончить запись, не дожидаясь, пока файл допишется.
    pub fn stop(mut self) -> Option<JoinHandle<()>> {
        self.tx = None;
        self.thread.take()
    }
}

impl Drop for LiveRec {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn write_loop(
    path: &Path,
    names: &[String],
    silence: &[u8],
    width: u32,
    height: u32,
    pre: (Vec<Arc<Packet>>, Vec<Vec<Arc<AudioPacket>>>),
    rx: Receiver<Msg>,
) -> Result<(PathBuf, f64, u64)> {
    let mut w: Option<LiveWriter> = None;
    // звук, пришедший раньше первого ключевого кадра
    let mut early: Vec<(usize, Arc<AudioPacket>)> = Vec::new();
    let (pre_video, pre_audio) = pre;
    let msgs = pre_video
        .into_iter()
        .map(Msg::Video)
        .chain(pre_audio.into_iter().enumerate().flat_map(|(k, q)| q.into_iter().map(move |p| Msg::Audio(k, p))))
        .chain(rx.iter());
    for m in msgs {
        match m {
            Msg::Video(p) => match &mut w {
                Some(w) => w.video(p)?,
                None if p.key => {
                    let mut lw = LiveWriter::create(path, &p, names, silence, width, height)?;
                    lw.video(p)?;
                    for (k, a) in early.drain(..) {
                        lw.audio(k, &a)?;
                    }
                    w = Some(lw);
                }
                None => {}
            },
            Msg::Audio(k, a) => match &mut w {
                Some(w) => w.audio(k, &a)?,
                None => {
                    early.push((k, a));
                    if early.len() > 2000 {
                        early.drain(..1000);
                    }
                }
            },
        }
    }
    let Some(w) = w else { bail!("запись слишком короткая — не пришло ни одного кадра") };
    let (path, secs) = w.finish()?;
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok((path, secs, bytes))
}

#[cfg(test)]
#[path = "tests/live.rs"]
mod tests;
