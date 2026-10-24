//! Монтаж: несколько кусков клипа подряд в один файл, точно по кадрам.
//!
//! Какие кадры попадают в кусок, решается заранее по таблице кадров MP4 (`plan`): кадр берётся,
//! если его середина внутри куска, — так соседние куски после разреза делят кадры без повторов.
//! По этому плану сначала собирается звук: каждая дорожка раскодируется кусками, склеивается
//! точно по отсчётам и кодируется заново (громкость и «Всё вместе» — как при сохранении отрезка).
//! Потом видео перекодируется на видеокарте и сразу пишется в файл вместе со звуком —
//! в памяти не держится. В конце оглавление MP4 переносится в начало (`faststart`).

use crate::aac::AudioPacket;
use crate::convert::Converter;
use crate::encoder::{Encoder, EncoderSettings, Packet};
use crate::media::{self, MfScope, Quality};
use crate::mux::{self, AudioInput, Writer, TIMESCALE};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use mp4::Mp4Sample;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;

/// Кусок исходника в готовом видео. Время — в единицах 100 нс.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    /// Начало первого и конец последнего взятого кадра в исходнике.
    pub src_start: i64,
    pub src_end: i64,
    /// Где кусок начинается в готовом видео.
    pub out_start: i64,
    pub frames: usize,
}

/// Что получилось.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MontageOut {
    /// Длина готового видео, с.
    pub duration: f64,
    pub pieces: usize,
}

/// Кадры, которые войдут в монтаж. `times` — начала кадров исходника по порядку, `end` — конец
/// последнего (100 нс), `ranges` — оставленные куски в секундах (порядок и наложения не важны).
/// Соседние куски, между которыми не осталось ни одного кадра, сливаются в один.
pub fn plan(times: &[i64], end: i64, ranges: &[(f64, f64)]) -> Vec<Piece> {
    let mut r: Vec<(f64, f64)> = ranges.iter().copied().filter(|(s, e)| s.is_finite() && e.is_finite() && e > s).collect();
    r.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (s, e) in r {
        match merged.last_mut() {
            Some(m) if s <= m.1 => m.1 = m.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let n = times.len();
    let next = |k: usize| if k + 1 < n { times[k + 1] } else { end };
    // середины кадров (удвоенные — без дробей) растут вместе с кадрами
    let mid2: Vec<i64> = (0..n).map(|k| times[k] + next(k)).collect();
    let mut out: Vec<Piece> = Vec::new();
    let mut acc = 0i64;
    let mut last_excl = usize::MAX;
    for (s, e) in merged {
        let (a, b) = ((s * 1e7).round() as i64 * 2, (e * 1e7).round() as i64 * 2);
        let first = mid2.partition_point(|&m| m < a);
        let stop = mid2.partition_point(|&m| m < b);
        if first >= stop {
            continue;
        }
        let (src_start, src_end) = (times[first], next(stop - 1));
        match out.last_mut() {
            // кадры идут подряд — перематывать незачем
            Some(p) if last_excl == first => {
                p.src_end = src_end;
                p.frames += stop - first;
            }
            _ => out.push(Piece { src_start, src_end, out_start: acc, frames: stop - first }),
        }
        acc = out.last().map_or(0, |p| p.out_start + p.src_end - p.src_start);
        last_excl = stop;
    }
    out
}

/// Длина готового видео (100 нс).
pub fn total(pieces: &[Piece]) -> i64 {
    pieces.last().map_or(0, |p| p.out_start + p.src_end - p.src_start)
}

/// Время готового видео (100 нс) -> номер отсчёта 48 кГц. Границы считаются от общего начала,
/// поэтому округления по кускам не накапливаются.
fn samples_at(t: i64) -> i64 {
    ((t as i128 * crate::audio::RATE as i128 + 5_000_000) / 10_000_000) as i64
}

/// Начала кадров видеодорожки и конец последнего (100 нс) — по таблице MP4, без декодирования.
pub fn frame_times(src: &Path) -> Result<(Vec<i64>, i64)> {
    let r = mux::open(src)?;
    let id = mux::video_track_id(&r)?;
    let t = &r.tracks()[&id];
    let ts = t.timescale().max(1) as i128;
    let to = |x: u64| ((x as i128 * 10_000_000 + ts / 2) / ts) as i64;
    let mut times = Vec::with_capacity(t.sample_count() as usize);
    let mut acc = 0u64;
    for e in &t.trak.mdia.minf.stbl.stts.entries {
        for _ in 0..e.sample_count {
            times.push(to(acc));
            acc += e.sample_delta as u64;
        }
    }
    times.truncate(t.sample_count() as usize);
    Ok((times, to(acc)))
}

/// «Оригинал»: то же разрешение и битрейт исходника с запасом на повторное сжатие.
pub fn source_quality(src: &Path) -> Result<Quality> {
    let r = mux::open(src)?;
    let id = mux::video_track_id(&r)?;
    let t = &r.tracks()[&id];
    let secs = t.duration().as_secs_f64();
    let stsz = &t.trak.mdia.minf.stbl.stsz;
    let bytes: u64 = if stsz.sample_size > 0 { stsz.sample_size as u64 * stsz.sample_count as u64 } else { stsz.sample_sizes.iter().map(|&x| x as u64).sum() };
    let bps = if secs > 0.0 { bytes as f64 * 8.0 / secs } else { 20e6 };
    Ok(Quality { max_height: t.height() as u32, bits_per_pixel: 0.0, fixed_bitrate: Some((bps * 1.15).clamp(2e6, 80e6) as u32) })
}

fn check(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("отменено");
    }
    Ok(())
}

/// Сохранить монтаж: куски `ranges` (секунды исходника) подряд в `dst`.
/// Звук: дорожки `audio_keep` (None — все) с громкостью `audio_gains`, как у отрезка.
#[allow(clippy::too_many_arguments)]
pub fn montage(
    src: &Path,
    dst: &Path,
    ranges: &[(f64, f64)],
    audio_keep: Option<&[usize]>,
    audio_gains: Option<&[f32]>,
    q: &Quality,
    progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> Result<MontageOut> {
    let (times, end) = frame_times(src)?;
    let pieces = plan(&times, end, ranges);
    if pieces.is_empty() {
        bail!("в кусках нет ни одного кадра");
    }
    let info = mux::clip_info(src)?;
    let part = dst.with_extension("part");
    let fast = dst.with_extension("fs.part");
    let r = (|| {
        let _mf = MfScope::new()?;
        let (tracks, names) = build_audio(src, &pieces, &info.audio_names, audio_keep, audio_gains, &mut |f| progress(f * 0.1), cancel)?;
        encode_video(src, &part, &pieces, &info, tracks, &names, q, &mut |f| progress(0.1 + f * 0.85), cancel)?;
        check(cancel)?;
        faststart(&part, &fast)?;
        let _ = std::fs::remove_file(&part);
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    mux::finish(&fast, dst, r)?;
    progress(1.0);
    Ok(MontageOut { duration: total(&pieces) as f64 / 1e7, pieces: pieces.len() })
}

// ---------- звук ----------

/// Сколько звука читается перед каждым куском: декодер AAC «разогревается» на нём.
const PRE: i64 = 1_000_000;
/// Кусок исходника раскодируется частями не длиннее 20 с — память не растёт с длиной клипа.
const CHUNK: i64 = 200_000_000;

/// `n` стерео-отсчётов звука, начиная с отсчёта `from` (в системе позиций кадров `p0`); чего нет — тишина.
fn take(p0: i64, pcm: &[f32], from: i64, n: usize) -> Vec<f32> {
    let mut out = vec![0f32; n * 2];
    let have = pcm.len() as i64 / 2;
    for (k, o) in out.chunks_mut(2).enumerate() {
        let i = from + k as i64 - p0;
        if i >= 0 && i < have {
            o.copy_from_slice(&pcm[i as usize * 2..i as usize * 2 + 2]);
        }
    }
    out
}

fn unchanged(g: f32) -> bool {
    (g - 1.0).abs() < 1e-3
}

/// Звуковые дорожки монтажа: кадры AAC с позициями от начала готового видео и их названия.
fn build_audio(
    src: &Path,
    pieces: &[Piece],
    names: &[String],
    keep: Option<&[usize]>,
    gains: Option<&[f32]>,
    progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> Result<(Vec<Vec<Arc<AudioPacket>>>, Vec<String>)> {
    let keep: Vec<usize> = match keep {
        Some(k) => k.iter().copied().filter(|&i| i < names.len()).collect(),
        None => (0..names.len()).collect(),
    };
    if keep.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let gain = |i: usize| gains.and_then(|g| g.get(i)).copied().unwrap_or(1.0).clamp(0.0, 4.0);
    // «Всё вместе» сводится заново (с выравниванием голоса), если громкость системы,
    // микрофона или её самой менялась, или клип записан до выравнивания
    let remix = if keep.contains(&0) { media::remix_needed(src, names, &gain) } else { None };
    let mut need = keep.clone();
    if let Some((s, m)) = remix {
        for i in [s, m] {
            if !need.contains(&i) {
                need.push(i);
            }
        }
    }

    // уровни голоса и игры меряются по первому куску (до 20 с) — дальше выравнивание подстраивается
    let mut voice: Option<crate::voicemix::VoiceMix> = None;
    let mut encs = keep.iter().map(|_| crate::aac::AacEncoder::new()).collect::<Result<Vec<_>>>()?;
    let mut out: Vec<Vec<AudioPacket>> = keep.iter().map(|_| Vec::new()).collect();
    let all = total(pieces).max(1);
    let pre = samples_at(PRE);
    for p in pieces {
        let mut cs = p.src_start;
        while cs < p.src_end {
            check(cancel)?;
            let ce = (cs + CHUNK).min(p.src_end);
            let os = p.out_start + (cs - p.src_start);
            let pos = samples_at(os);
            let n = (samples_at(os + (ce - cs)) - pos).max(0) as usize;
            let (tracks, _) = mux::read_audio_segment(src, (cs - PRE) as f64 / 1e7, (ce + PRE) as f64 / 1e7, Some(&need))?;
            let mut pcm: Vec<Option<Vec<f32>>> = vec![None; names.len()];
            for (k, &i) in need.iter().enumerate() {
                let (p0, v) = media::decode_track(&tracks[k])?;
                pcm[i] = Some(take(p0, &v, pre, n));
            }
            for (j, &i) in keep.iter().enumerate() {
                let buf = match remix {
                    Some((s, m)) if i == 0 => {
                        let (a, b) = (pcm[s].as_ref().unwrap(), pcm[m].as_ref().unwrap());
                        let vm = voice.get_or_insert_with(|| crate::voicemix::VoiceMix::primed(a, b));
                        let mut v = vm.process(a, b, gain(s), gain(m));
                        let g0 = gain(0);
                        if !unchanged(g0) {
                            v.iter_mut().for_each(|x| *x = crate::mixer::soft_clip(*x * g0));
                        }
                        v
                    }
                    _ => {
                        let g = gain(i);
                        let v = pcm[i].as_ref().unwrap();
                        if unchanged(g) {
                            v.clone()
                        } else {
                            // громче исходного — мягкое ограничение, как при записи
                            v.iter().map(|x| if g > 1.0 { crate::mixer::soft_clip(x * g) } else { x * g }).collect()
                        }
                    }
                };
                encs[j].encode(pos, &buf, &mut out[j])?;
            }
            progress((os + (ce - cs)) as f64 / all as f64);
            cs = ce;
        }
    }
    // немного тишины в конце, иначе кодировщик придержит последние кадры (лишнее отрежет сборка)
    let end = samples_at(total(pieces));
    for (enc, o) in encs.iter_mut().zip(out.iter_mut()) {
        enc.encode(end, &vec![0.0; crate::aac::FRAME as usize * 2 * 4], o)?;
    }
    let tracks = out.into_iter().map(|v| v.into_iter().map(Arc::new).collect()).collect();
    Ok((tracks, keep.iter().map(|&i| names[i].clone()).collect()))
}

// ---------- видео ----------

#[allow(clippy::too_many_arguments)]
fn encode_video(
    src: &Path,
    part: &Path,
    pieces: &[Piece],
    info: &mux::ClipInfo,
    audio: Vec<Vec<Arc<AudioPacket>>>,
    names: &[String],
    q: &Quality,
    progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> Result<()> {
    let fps = if info.fps > 1.0 { info.fps } else { 60.0 };
    let (out_w, out_h) = q.out_size(info.width, info.height);
    let bitrate = q.bitrate(out_w, out_h, fps);
    let silence = if audio.is_empty() { Vec::new() } else { crate::aac::silent_frame()? };
    let mut out = StreamWriter::new(part, out_w, out_h, &audio, &silence, total(pieces));
    let frames_all: usize = pieces.iter().map(|p| p.frames).sum::<usize>().max(1);
    let mut done = 0usize;
    let (dev, ctx) = media::device()?;
    unsafe {
        let mut token = 0u32;
        let mut mgr = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
        let mgr = mgr.context("нет диспетчера устройств")?;
        mgr.ResetDevice(&dev, token)?;
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.context("атрибуты")?;
        attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &mgr)?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        let reader = MFCreateSourceReaderFromURL(PCWSTR(media::wide(src).as_ptr()), &attrs).context("клип не открылся")?;
        reader.SetStreamSelection(media::ALL_STREAMS, false)?;
        reader.SetStreamSelection(media::FIRST_VIDEO, true)?;
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        reader.SetCurrentMediaType(media::FIRST_VIDEO, None, &t).context("декодер не выдаёт NV12")?;

        let frame_dur = (1e7 / fps) as i64;
        let tol = frame_dur / 2;
        let mut enc = Encoder::new(&dev, &EncoderSettings { width: out_w, height: out_h, fps: fps.round() as u32, bitrate })?;
        let conv = Converter::for_video(&dev, &ctx, info.width, info.height, out_w, out_h, fps.round() as u32)?;
        let pool = (0..8).map(|_| conv.make_target(&dev)).collect::<Result<Vec<_>>>()?;
        let mut pi = 0;
        let mut ready: Vec<Packet> = Vec::new();
        let mut mismatch = 0i64;

        // PROPVARIANT с типом VT_I8 (время в 100 нс) — позиция для перемотки
        #[repr(C)]
        struct I8Variant {
            vt: u16,
            r: [u16; 3],
            value: i64,
            pad: i64,
        }
        for p in pieces {
            let pos = I8Variant { vt: 20, r: [0; 3], value: p.src_start, pad: 0 };
            reader.SetCurrentPosition(&GUID::zeroed(), &pos as *const I8Variant as *const _)?;
            let mut taken = 0usize;
            loop {
                check(cancel)?;
                let mut flags = 0u32;
                let mut ts = 0i64;
                let mut sample = None;
                reader.ReadSample(media::FIRST_VIDEO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample))?;
                if flags & media::EOS != 0 {
                    break;
                }
                let Some(sample) = sample else { continue };
                // перемотка встаёт на ключевой кадр раньше — кадры до куска пропускаем
                if ts < p.src_start - tol {
                    continue;
                }
                if ts >= p.src_end - tol || taken >= p.frames {
                    break;
                }
                let buf = sample.GetBufferByIndex(0)?;
                let dxgi: IMFDXGIBuffer = buf.cast().context("декодер работает не на видеокарте")?;
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                let tex = ID3D11Texture2D::from_raw(raw);
                let slice = dxgi.GetSubresourceIndex()?;

                enc.wait_ready(&mut |pk| ready.push(pk))?;
                let (target, view) = &pool[pi];
                conv.convert_texture(&tex, slice, view)?;
                enc.push_at(target, p.out_start + (ts - p.src_start), frame_dur)?;
                pi = (pi + 1) % pool.len();
                taken += 1;
                done += 1;
                for pk in ready.drain(..) {
                    out.push(pk)?;
                }
                progress(done as f64 / frames_all as f64);
            }
            mismatch += taken as i64 - p.frames as i64;
        }
        enc.finish(&mut |pk| ready.push(pk))?;
        drop(reader);
        for pk in ready.drain(..) {
            out.push(pk)?;
        }
        if mismatch != 0 {
            crate::util::log(format!("монтаж: кадров вышло на {mismatch} не столько, сколько в плане"));
        }
    }
    out.finish(names)
}

/// Запись MP4 по мере готовности видеокадров: звук (готовый заранее) вставляется между ними
/// по времени, как в `mux::write_av_named`, но кадры не копятся в памяти.
pub(crate) struct StreamWriter<'a> {
    path: &'a Path,
    width: u32,
    height: u32,
    w: Option<Writer>,
    audio: Vec<Vec<(i64, Bytes)>>,
    next: Vec<usize>,
    /// Пакеты кодировщика ждут здесь, пока не станет ясен порядок по времени.
    reorder: Vec<Packet>,
    /// Последний кадр ждёт следующего — так известна его длительность.
    held: Option<Packet>,
    end: u64,
}

const REORDER: usize = 4;

fn to_ts(pts: i64) -> u64 {
    (pts.max(0) as u64 * TIMESCALE + 5_000_000) / 10_000_000
}

impl<'a> StreamWriter<'a> {
    /// `total` — длина видео (100 нс); звук подгоняется ровно под неё.
    pub(crate) fn new(path: &'a Path, width: u32, height: u32, audio: &[Vec<Arc<AudioPacket>>], silence: &[u8], total: i64) -> Self {
        let end_samples = samples_at(total);
        let audio: Vec<_> = audio.iter().map(|a| mux::layout_audio(&AudioInput { packets: a, silence }, 0, end_samples)).collect();
        let next = vec![0; audio.len()];
        StreamWriter { path, width, height, w: None, audio, next, reorder: Vec::new(), held: None, end: to_ts(total) }
    }

    pub(crate) fn push(&mut self, p: Packet) -> Result<()> {
        let at = self.reorder.partition_point(|x| x.pts <= p.pts);
        self.reorder.insert(at, p);
        if self.reorder.len() > REORDER {
            let first = self.reorder.remove(0);
            self.write(first)?;
        }
        Ok(())
    }

    fn write(&mut self, p: Packet) -> Result<()> {
        if self.w.is_none() {
            if !p.key {
                bail!("кодировщик не начал с ключевого кадра");
            }
            let Some((sps, pps)) = mux::find_sps_pps(&p.data) else { bail!("в первом кадре нет SPS/PPS") };
            let mut w = mux::new_writer(self.path)?;
            mux::add_video(&mut w, self.width, self.height, sps, pps, TIMESCALE as u32)?;
            for _ in &self.audio {
                mux::add_audio(&mut w, mux::our_aac(), crate::audio::RATE)?;
            }
            self.w = Some(w);
        }
        if let Some(h) = self.held.take() {
            let dur = to_ts(p.pts).saturating_sub(to_ts(h.pts)).max(1);
            self.write_video(h, dur)?;
        }
        self.held = Some(p);
        Ok(())
    }

    fn write_video(&mut self, p: Packet, dur: u64) -> Result<()> {
        let w = self.w.as_mut().unwrap();
        let start = to_ts(p.pts);
        let rate = crate::audio::RATE as u64;
        // сначала звук, который начинается не позже этого кадра
        for (k, t) in self.audio.iter().enumerate() {
            while let Some((pos, data)) = t.get(self.next[k]) {
                if (*pos as u64) * TIMESCALE > start * rate {
                    break;
                }
                w.write_sample(k as u32 + 2, &mux::audio_sample(*pos, data.clone()))?;
                self.next[k] += 1;
            }
        }
        w.write_sample(1, &Mp4Sample { start_time: start, duration: dur as u32, rendering_offset: 0, is_sync: p.key, bytes: Bytes::from(mux::annexb_to_avcc(&p.data)) })?;
        Ok(())
    }

    pub(crate) fn finish(mut self, names: &[String]) -> Result<()> {
        for p in std::mem::take(&mut self.reorder) {
            self.write(p)?;
        }
        let Some(h) = self.held.take() else { bail!("в монтаже нет кадров") };
        let dur = self.end.saturating_sub(to_ts(h.pts)).max(1);
        self.write_video(h, dur)?;
        let mut w = self.w.take().unwrap();
        for (k, t) in self.audio.iter().enumerate() {
            for (pos, data) in &t[self.next[k]..] {
                w.write_sample(k as u32 + 2, &mux::audio_sample(*pos, data.clone()))?;
            }
        }
        w.write_end()?;
        drop(w);
        if !names.is_empty() && names.len() == self.audio.len() {
            mux::finish_meta(self.path, names)?;
        }
        Ok(())
    }
}

// ---------- faststart ----------

/// Верхние блоки MP4: (тип, начало, размер).
pub(crate) fn top_boxes(f: &mut std::fs::File) -> Result<Vec<([u8; 4], u64, u64)>> {
    let len = f.metadata()?.len();
    let mut out = Vec::new();
    let mut pos = 0u64;
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 8];
        f.read_exact(&mut h)?;
        let mut size = u32::from_be_bytes(h[..4].try_into()?) as u64;
        if size == 1 {
            let mut big = [0u8; 8];
            f.read_exact(&mut big)?;
            size = u64::from_be_bytes(big);
        } else if size == 0 {
            size = len - pos;
        }
        if size < 8 || pos + size > len {
            bail!("MP4 повреждён: блок {} на {pos}", String::from_utf8_lossy(&h[4..]));
        }
        out.push((h[4..8].try_into()?, pos, size));
        pos += size;
    }
    Ok(out)
}

/// Сдвинуть ссылки на данные (stco / co64) внутри moov на `shift` байт.
fn shift_offsets(b: &mut [u8], shift: u64) -> Result<()> {
    let mut pos = 0usize;
    while pos + 8 <= b.len() {
        let size = u32::from_be_bytes(b[pos..pos + 4].try_into()?) as usize;
        if size < 8 || pos + size > b.len() {
            bail!("moov повреждён");
        }
        let kind = &b[pos + 4..pos + 8];
        let body = pos + 8;
        match kind {
            b"trak" | b"mdia" | b"minf" | b"stbl" => shift_offsets(&mut b[body..pos + size], shift)?,
            b"stco" | b"co64" => {
                let wide = kind == b"co64";
                let n = u32::from_be_bytes(b[body + 4..body + 8].try_into()?) as usize;
                let w = if wide { 8 } else { 4 };
                if body + 8 + n * w > pos + size {
                    bail!("moov повреждён");
                }
                for k in 0..n {
                    let at = body + 8 + k * w;
                    if wide {
                        let v = u64::from_be_bytes(b[at..at + 8].try_into()?) + shift;
                        b[at..at + 8].copy_from_slice(&v.to_be_bytes());
                    } else {
                        let v = u32::from_be_bytes(b[at..at + 4].try_into()?) as u64 + shift;
                        let v: u32 = v.try_into().map_err(|_| anyhow::anyhow!("файл больше 4 ГБ"))?;
                        b[at..at + 4].copy_from_slice(&v.to_be_bytes());
                    }
                }
            }
            _ => {}
        }
        pos += size;
    }
    Ok(())
}

/// Переписать `src` в `dst` с оглавлением (moov) перед данными: плееры и Discord начинают
/// показывать видео, не дочитав файл до конца. Если moov и так впереди — просто копия.
pub fn faststart(src: &Path, dst: &Path) -> Result<()> {
    let mut f = std::fs::File::open(src)?;
    let boxes = top_boxes(&mut f)?;
    let moov = boxes.iter().find(|b| &b.0 == b"moov").context("в MP4 нет moov")?.clone();
    let mdat = boxes.iter().position(|b| &b.0 == b"mdat").context("в MP4 нет mdat")?;
    let first_data = boxes[mdat].1;
    let mut m = vec![0u8; moov.2 as usize];
    f.seek(SeekFrom::Start(moov.1))?;
    f.read_exact(&mut m)?;
    let moov_ahead = moov.1 < first_data;
    if !moov_ahead {
        shift_offsets(&mut m[8..], moov.2)?;
    }
    let mut o = std::io::BufWriter::new(std::fs::File::create(dst).with_context(|| format!("не удалось создать {}", dst.display()))?);
    let copy = |f: &mut std::fs::File, o: &mut std::io::BufWriter<std::fs::File>, from: u64, len: u64| -> Result<()> {
        f.seek(SeekFrom::Start(from))?;
        std::io::copy(&mut Read::take(&mut *f, len), o)?;
        Ok(())
    };
    // всё до данных (ftyp), затем moov, затем остальное по порядку
    copy(&mut f, &mut o, 0, first_data)?;
    if !moov_ahead {
        o.write_all(&m)?;
    }
    for b in boxes.iter().filter(|b| b.1 >= first_data && !(b.1 == moov.1 && !moov_ahead)) {
        copy(&mut f, &mut o, b.1, b.2)?;
    }
    o.flush()?;
    Ok(())
}

/// Место для результата рядом с оригиналом: «<имя> (монтаж).mp4».
pub fn out_path(src: &Path) -> Result<PathBuf> {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = src.parent().context("нет папки")?;
    Ok(crate::util::unique_path(dir, &format!("{stem} (монтаж)"), "mp4"))
}

#[cfg(test)]
#[path = "tests/montage.rs"]
mod tests;
