//! Журнал полноценной записи: рядом с `.part` пишется `.idx` со списком кадров.
//!
//! Оглавление MP4 (`moov`) появляется только в конце записи. Если программа упала,
//! её закрыли из диспетчера задач или выключился свет, `.part` без оглавления не
//! открыть. По журналу кадры из `.part` собираются в нормальный MP4 (`recover`).

use crate::mux::{self, TIMESCALE};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use mp4::Mp4Sample;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"CBJ1";
/// Дорожка (1), начало (8), длительность (4), размер (4), ключевой (1).
const ENTRY: usize = 18;

pub fn path_for(part: &Path) -> PathBuf {
    part.with_extension("idx")
}

pub struct Journal {
    f: BufWriter<File>,
    flushed: Instant,
}

fn put_bytes(v: &mut Vec<u8>, b: &[u8]) {
    v.extend_from_slice(&(b.len() as u16).to_le_bytes());
    v.extend_from_slice(b);
}

impl Journal {
    pub fn create(part: &Path, width: u32, height: u32, sps: &[u8], pps: &[u8], names: &[String]) -> Result<Self> {
        let mut h = MAGIC.to_vec();
        h.extend_from_slice(&width.to_le_bytes());
        h.extend_from_slice(&height.to_le_bytes());
        put_bytes(&mut h, sps);
        put_bytes(&mut h, pps);
        h.push(names.len() as u8);
        for n in names {
            put_bytes(&mut h, n.as_bytes());
        }
        let mut f = BufWriter::new(File::create(path_for(part))?);
        f.write_all(&h)?;
        f.flush()?;
        Ok(Self { f, flushed: Instant::now() })
    }

    #[cfg(test)]
    pub fn flush_now(&mut self) {
        let _ = self.f.flush();
    }

    pub fn add(&mut self, track: u32, s: &Mp4Sample) -> Result<()> {
        let mut e = [0u8; ENTRY];
        e[0] = track as u8;
        e[1..9].copy_from_slice(&s.start_time.to_le_bytes());
        e[9..13].copy_from_slice(&s.duration.to_le_bytes());
        e[13..17].copy_from_slice(&(s.bytes.len() as u32).to_le_bytes());
        e[17] = s.is_sync as u8;
        self.f.write_all(&e)?;
        // на диск — раз в пару секунд: при сбое теряются только последние секунды
        if self.flushed.elapsed() >= Duration::from_secs(2) {
            self.f.flush()?;
            self.flushed = Instant::now();
        }
        Ok(())
    }
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if b.len() < n {
        bail!("журнал записи повреждён");
    }
    let (x, rest) = b.split_at(n);
    *b = rest;
    Ok(x)
}

fn take_bytes<'a>(b: &mut &'a [u8]) -> Result<&'a [u8]> {
    let n = u16::from_le_bytes(take(b, 2)?.try_into()?) as usize;
    take(b, n)
}

struct Entry {
    track: u32,
    start: u64,
    dur: u32,
    size: u32,
    sync: bool,
}

/// Собрать MP4 из `.part` по журналу. Возвращает путь клипа и его длину в секундах.
/// `.part` и журнал удаляются только после удачной сборки.
pub fn recover(part: &Path) -> Result<(PathBuf, f64)> {
    let idx = path_for(part);
    let j = std::fs::read(&idx).context("нет журнала записи")?;
    let mut b = j.as_slice();
    if take(&mut b, 4)? != MAGIC {
        bail!("журнал записи другой версии");
    }
    let width = u32::from_le_bytes(take(&mut b, 4)?.try_into()?);
    let height = u32::from_le_bytes(take(&mut b, 4)?.try_into()?);
    let sps = take_bytes(&mut b)?.to_vec();
    let pps = take_bytes(&mut b)?.to_vec();
    let n = take(&mut b, 1)?[0] as usize;
    let mut names = Vec::new();
    for _ in 0..n {
        names.push(String::from_utf8_lossy(take_bytes(&mut b)?).into_owned());
    }

    let mut src = BufReader::new(File::open(part)?);
    let len = src.get_ref().metadata()?.len();
    // данные начинаются сразу за ftyp и заголовками mdat + wide (по 8 байт)
    let mut head = [0u8; 4];
    src.read_exact(&mut head)?;
    let mut offset = u32::from_be_bytes(head) as u64 + 16;
    // библиотека mp4 копит кадры каждой дорожки и пишет их кусками по секунде
    // (`duration_per_chunk` = масштаб времени дорожки); недописанный кусок пропал вместе с программой.
    // Повторяем ту же раскладку: куски в файле идут в порядке заполнения.
    let scale = |t: u32| if t == 1 { TIMESCALE as u32 } else { crate::audio::RATE };
    let mut pend: Vec<(Vec<Entry>, u32)> = (0..n + 2).map(|_| (Vec::new(), 0)).collect();
    let mut entries = Vec::new();
    for e in b.chunks_exact(ENTRY) {
        let x = Entry {
            track: e[0] as u32,
            start: u64::from_le_bytes(e[1..9].try_into()?),
            dur: u32::from_le_bytes(e[9..13].try_into()?),
            size: u32::from_le_bytes(e[13..17].try_into()?),
            sync: e[17] != 0,
        };
        if x.track == 0 || x.track as usize > n + 1 {
            break;
        }
        let t = x.track;
        let p = &mut pend[t as usize];
        p.1 = p.1.saturating_add(x.dur);
        p.0.push(x);
        if p.1 >= scale(t) {
            let size: u64 = p.0.iter().map(|e| e.size as u64).sum();
            // журнал может опережать данные: файл записи сбрасывается на диск своим буфером
            if offset + size > len {
                break;
            }
            offset += size;
            entries.append(&mut p.0);
            p.1 = 0;
        }
    }
    let Some(last_video) = entries.iter().rev().find(|e| e.track == 1) else { bail!("в записи нет ни одного кадра") };
    let end = last_video.start + last_video.dur as u64;
    let video_end_samples = end * crate::audio::RATE as u64 / TIMESCALE;

    let out = free_name(&part.with_extension("mp4"));
    let tmp = part.with_extension("fix");
    src.seek(SeekFrom::Start(u32::from_be_bytes(head) as u64 + 16))?;
    let r = (|| {
        let mut w = mux::new_writer(&tmp)?;
        mux::add_video(&mut w, width, height, sps, pps, TIMESCALE as u32)?;
        for _ in 0..n {
            mux::add_audio(&mut w, mux::our_aac(), crate::audio::RATE)?;
        }
        let mut buf = Vec::new();
        for e in &entries {
            buf.resize(e.size as usize, 0);
            src.read_exact(&mut buf)?;
            // звук не длиннее видео — плееры иначе показывают лишнюю тишину в конце
            if e.track > 1 && e.start >= video_end_samples {
                continue;
            }
            let s = Mp4Sample { start_time: e.start, duration: e.dur, rendering_offset: 0, is_sync: e.sync, bytes: Bytes::copy_from_slice(&buf) };
            w.write_sample(e.track, &s)?;
        }
        w.write_end()?;
        drop(w);
        mux::finish_meta(&tmp, &names)?;
        Ok(())
    })();
    drop(src);
    mux::finish(&tmp, &out, r)?;
    let _ = std::fs::remove_file(part);
    let _ = std::fs::remove_file(&idx);
    Ok((out, end as f64 / TIMESCALE as f64))
}

/// `name.mp4`, а если такой уже есть — `name (2).mp4` и т. д.
fn free_name(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    (2..).map(|k| p.with_file_name(format!("{stem} ({k}).mp4"))).find(|q| !q.exists()).unwrap()
}
