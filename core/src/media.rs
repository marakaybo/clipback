//! Работа с готовыми клипами через декодер Windows (Media Foundation):
//! форма волны звуковых дорожек и сохранение отрезка в другом качестве (перекодирование на видеокарте).

use crate::aac::AudioPacket;
use crate::convert::Converter;
use crate::encoder::{Encoder, EncoderSettings, Packet};
use crate::mux::{self, TrimResult};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::Arc;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Media Foundation в текущем потоке (счётчик внутри MF, лишний вызов безопасен).
pub(crate) struct MfScope;
impl MfScope {
    pub(crate) fn new() -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        }
        Ok(MfScope)
    }
}
impl Drop for MfScope {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

pub(crate) fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain([0]).collect()
}

pub(crate) const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
pub(crate) const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
pub(crate) const EOS: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;

/// Насколько звук на выходе декодера отстаёт от позиции первого поданного кадра (отсчёты 48 кГц).
/// Проверяется тестом `aac_round_trip_delay` вместе с кодировщиком Clipback.
const DECODE_DELAY: i64 = 0;

/// Декодер AAC Windows: сырые кадры AAC-LC 48 кГц стерео -> PCM 16 бит.
struct AacDecoder {
    mft: IMFTransform,
    out_size: u32,
}

impl AacDecoder {
    unsafe fn new() -> Result<Self> {
        let mft: IMFTransform = windows::Win32::System::Com::CoCreateInstance(&CLSID_MSAACDecMFT, None, windows::Win32::System::Com::CLSCTX_INPROC_SERVER)
            .context("нет AAC-декодера Windows")?;
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, crate::audio::RATE)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
        // HEAACWAVEINFO (без WAVEFORMATEX) + AudioSpecificConfig: AAC-LC, 48 кГц, стерео
        let user: [u8; 14] = [0, 0, 0x29, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x90];
        t.SetBlob(&MF_MT_USER_DATA, &user)?;
        mft.SetInputType(0, &t, 0).context("AAC-декодер не принял формат")?;
        let mut set = false;
        for i in 0..16 {
            let Ok(o) = mft.GetOutputAvailableType(0, i) else { break };
            if o.GetGUID(&MF_MT_SUBTYPE)? == MFAudioFormat_PCM && o.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(0) == 16 {
                mft.SetOutputType(0, &o, 0)?;
                set = true;
                break;
            }
        }
        if !set {
            bail!("AAC-декодер не выдаёт PCM 16 бит");
        }
        let info = mft.GetOutputStreamInfo(0)?;
        Ok(Self { mft, out_size: info.cbSize.max(16384) })
    }

    /// Раскодировать кадр; для каждого полученного стерео-кадра вызывается `f(пик)`.
    unsafe fn decode(&mut self, data: &[u8], f: &mut dyn FnMut(f32)) -> Result<()> {
        self.feed(data, &mut |pcm| {
            for fr in pcm.chunks(2) {
                f(fr.iter().map(|&x| (x as f32 / 32768.0).abs()).fold(0f32, f32::max));
            }
        })
    }

    /// Раскодировать кадр в стерео −1…1 и дописать в `out`.
    unsafe fn decode_pcm(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<()> {
        self.feed(data, &mut |pcm| out.extend(pcm.iter().map(|&x| x as f32 / 32768.0)))
    }

    /// Подать кадр AAC; каждый готовый кусок PCM (16 бит, стерео) отдаётся в `f`.
    unsafe fn feed(&mut self, data: &[u8], f: &mut dyn FnMut(&[i16])) -> Result<()> {
        let buf = MFCreateMemoryBuffer(data.len() as u32)?;
        let mut p = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        buf.Unlock()?;
        buf.SetCurrentLength(data.len() as u32)?;
        let s = MFCreateSample()?;
        s.AddBuffer(&buf)?;
        self.mft.ProcessInput(0, &s, 0)?;
        loop {
            let os = MFCreateSample()?;
            os.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
            let mut db = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: std::mem::ManuallyDrop::new(Some(os)), dwStatus: 0, pEvents: std::mem::ManuallyDrop::new(None) }];
            let mut st = 0;
            let r = self.mft.ProcessOutput(0, &mut db, &mut st);
            let sample = std::mem::ManuallyDrop::take(&mut db[0].pSample);
            drop(std::mem::ManuallyDrop::take(&mut db[0].pEvents));
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let Some(sample) = sample else { continue };
            let b = sample.ConvertToContiguousBuffer()?;
            let mut q = std::ptr::null_mut();
            let mut len = 0u32;
            b.Lock(&mut q, None, Some(&mut len))?;
            f(std::slice::from_raw_parts(q as *const i16, len as usize / 2));
            b.Unlock()?;
        }
    }
}

/// Форма волны: для каждой звуковой дорожки — `bins` пиков 0..1 по всей длине клипа.
/// Кадры AAC берутся прямо из файла (по номерам дорожек, как везде в Clipback).
pub fn audio_peaks(path: &Path, bins: usize, duration: f64) -> Result<Vec<Vec<f32>>> {
    let _mf = MfScope::new()?;
    let bins = bins.clamp(16, 4000);
    let (tracks, _) = mux::read_audio_segment(path, 0.0, duration.max(0.001) + 1.0, None)?;
    let total = duration.max(0.001) * crate::audio::RATE as f64;
    let mut out = Vec::new();
    for t in tracks {
        let mut peaks = vec![0f32; bins];
        let mut dec = unsafe { AacDecoder::new()? };
        // время берём по счётчику кадров: декодер выдаёт звук подряд
        let mut n: i64 = t.first().map_or(0, |p| p.pos.max(0));
        for p in &t {
            unsafe {
                dec.decode(&p.data, &mut |v| {
                    let b = (n as f64 / total * bins as f64) as usize;
                    if b < bins && v > peaks[b] {
                        peaks[b] = v;
                    }
                    n += 1;
                })?;
            }
        }
        out.push(peaks);
    }
    Ok(out)
}

/// Громкость дорожки «как есть».
fn unchanged(g: f32) -> bool {
    (g - 1.0).abs() < 1e-3
}

/// Звук отрезка [start; end) секунд: кадры AAC дорожек `keep` (None — всех) и их названия,
/// позиции — в отсчётах 48 кГц от `start`.
/// `gains` — громкость каждой дорожки файла по номеру (1 — как есть, 0 — тишина, до 4).
/// Пока все громкости 1, кадры копируются без перекодирования. Иначе изменённые дорожки
/// раскодируются, умножаются и кодируются заново, а «Всё вместе» сводится заново из «Звука
/// системы» и «Микрофона» с их громкостями — так же, как при записи (`mixer::render_track`).
pub fn audio_segment(
    src: &Path,
    start: f64,
    end: f64,
    keep: Option<&[usize]>,
    gains: Option<&[f32]>,
) -> Result<(Vec<Vec<Arc<AudioPacket>>>, Vec<String>)> {
    let Some(gains) = gains.filter(|g| g.iter().any(|&x| !unchanged(x))) else {
        return mux::read_audio_segment(src, start, end, keep);
    };
    let (all, names) = mux::read_audio_segment(src, start, end, None)?;
    let gain = |i: usize| gains.get(i).copied().unwrap_or(1.0).clamp(0.0, 4.0);
    let idx: Vec<usize> = match keep {
        Some(k) => k.iter().copied().filter(|&i| i < all.len()).collect(),
        None => (0..all.len()).collect(),
    };
    let sys = names.iter().position(|n| n == "Звук системы");
    let mic = names.iter().position(|n| n == "Микрофон");
    let mix_sources = match (names.first().map(String::as_str), sys, mic) {
        (Some("Всё вместе"), Some(s), Some(m)) => Some((s, m)),
        _ => None,
    };

    let _mf = MfScope::new()?;
    let mut tracks = Vec::new();
    let mut out_names = Vec::new();
    for i in idx {
        let packets = &all[i];
        let new = match mix_sources {
            Some((s, m)) if i == 0 => {
                if unchanged(gain(s)) && unchanged(gain(m)) && unchanged(gain(0)) {
                    None
                } else {
                    let (sp, sv) = decode_track(&all[s])?;
                    let (mp, mv) = decode_track(&all[m])?;
                    let from = sp.min(mp);
                    let len = ((sp + sv.len() as i64 / 2).max(mp + mv.len() as i64 / 2) - from).max(0) as usize;
                    let mut mix = vec![0f32; len * 2];
                    for (p0, v, g) in [(sp, &sv, gain(s)), (mp, &mv, gain(m))] {
                        let off = (p0 - from) as usize * 2;
                        for (k, x) in v.iter().enumerate() {
                            mix[off + k] += x * g;
                        }
                    }
                    let g0 = gain(0);
                    mix.iter_mut().for_each(|x| *x = crate::mixer::soft_clip(*x * g0));
                    Some(encode_track(from, &mix)?)
                }
            }
            _ if unchanged(gain(i)) => None,
            _ => {
                let (p0, mut v) = decode_track(packets)?;
                let g = gain(i);
                // громче исходного — мягкое ограничение, как при записи
                v.iter_mut().for_each(|x| *x = if g > 1.0 { crate::mixer::soft_clip(*x * g) } else { *x * g });
                Some(encode_track(p0, &v)?)
            }
        };
        tracks.push(new.unwrap_or_else(|| packets.clone()));
        out_names.push(names.get(i).cloned().unwrap_or_default());
    }
    Ok((tracks, out_names))
}

/// Раскодировать дорожку: позиция первого отсчёта и звук подряд (стерео −1…1).
pub(crate) fn decode_track(packets: &[Arc<AudioPacket>]) -> Result<(i64, Vec<f32>)> {
    let first = packets.first().map_or(0, |p| p.pos);
    let mut dec = unsafe { AacDecoder::new()? };
    let mut out = Vec::with_capacity(packets.len() * crate::aac::FRAME as usize * 2);
    for p in packets {
        unsafe { dec.decode_pcm(&p.data, &mut out)? };
    }
    Ok((first + DECODE_DELAY, out))
}

/// Закодировать звук обратно в кадры AAC с позициями от `pos`.
fn encode_track(pos: i64, samples: &[f32]) -> Result<Vec<Arc<AudioPacket>>> {
    let mut enc = crate::aac::AacEncoder::new()?;
    let mut out = Vec::new();
    // кусками по секунде; в конце — немного тишины, иначе кодировщик придержит последние кадры
    // (лишнее за концом видео отрежет сборка файла)
    let step = crate::audio::RATE as usize * 2;
    let mut p = pos;
    for chunk in samples.chunks(step) {
        enc.encode(p, chunk, &mut out)?;
        p += chunk.len() as i64 / 2;
    }
    enc.encode(p, &vec![0.0; crate::aac::FRAME as usize * 2 * 4], &mut out)?;
    Ok(out.into_iter().map(Arc::new).collect())
}

/// Качество сохранения отрезка.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quality {
    /// Не выше этой высоты (картинка уменьшается с сохранением пропорций).
    pub max_height: u32,
    /// Бит на пиксель кадра — от него считается битрейт.
    pub bits_per_pixel: f64,
    /// Точный битрейт видео (бит/с) — для сохранения под размер файла.
    pub fixed_bitrate: Option<u32>,
}

/// Звук при сжатии под размер: кадры AAC копируются как есть.
const AUDIO_BPS: f64 = crate::aac::BITRATE as f64;
/// Ниже этого видео превращается в кашу — лучше честно сказать, что отрезок слишком длинный.
const MIN_VIDEO_BPS: f64 = 250_000.0;
/// Запас на контейнер MP4 и на то, что кодировщик чуть превышает битрейт.
const SIZE_MARGIN: f64 = 0.92;

impl Quality {
    pub fn preset(name: &str) -> Option<Quality> {
        match name {
            "high" => Some(Quality { max_height: 1440, bits_per_pixel: 0.06, fixed_bitrate: None }),
            "medium" => Some(Quality { max_height: 1080, bits_per_pixel: 0.05, fixed_bitrate: None }),
            "low" => Some(Quality { max_height: 720, bits_per_pixel: 0.045, fixed_bitrate: None }),
            _ => None,
        }
    }

    /// Качество, при котором отрезок длиной `secs` с `audio_tracks` дорожками звука уложится в `bytes`.
    /// `squeeze` < 1 — ужать сильнее (повторная попытка, если файл всё же вышел больше).
    pub fn for_size(bytes: u64, secs: f64, audio_tracks: usize, squeeze: f64) -> Result<Quality> {
        let secs = secs.max(0.5);
        let budget = bytes as f64 * 8.0 * SIZE_MARGIN * squeeze;
        let video = budget / secs - AUDIO_BPS * audio_tracks as f64;
        if video < MIN_VIDEO_BPS {
            let max_secs = (bytes as f64 * 8.0 * SIZE_MARGIN) / (MIN_VIDEO_BPS + AUDIO_BPS * audio_tracks as f64);
            bail!(
                "в {} МБ поместится не больше {} с хорошего видео — сократите отрезок или выберите лимит больше",
                bytes / 1_000_000,
                max_secs.floor() as u64
            );
        }
        // разрешение — по битрейту: на маленьком битрейте 1440p выглядит хуже, чем честные 720p
        let max_height = match video {
            v if v >= 8_000_000.0 => 1440,
            v if v >= 4_000_000.0 => 1080,
            v if v >= 1_800_000.0 => 720,
            v if v >= 900_000.0 => 540,
            _ => 480,
        };
        Ok(Quality { max_height, bits_per_pixel: 0.0, fixed_bitrate: Some(video.min(80_000_000.0) as u32) })
    }

    /// Размер картинки на выходе (чётный).
    pub fn out_size(&self, w: u32, h: u32) -> (u32, u32) {
        let (w, h) = if h > self.max_height { ((w as u64 * self.max_height as u64 / h as u64) as u32, self.max_height) } else { (w, h) };
        (w.max(2) & !1, h.max(2) & !1)
    }

    /// Битрейт видео, бит/с.
    pub fn bitrate(&self, w: u32, h: u32, fps: f64) -> u32 {
        if let Some(b) = self.fixed_bitrate {
            return b;
        }
        ((w as f64 * h as f64 * fps.max(1.0) * self.bits_per_pixel) as u32).clamp(500_000, 80_000_000)
    }
}

/// Устройство D3D11 на основной видеокарте (для декодера и кодировщика).
pub(crate) fn device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut d = None;
    let mut c = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut d),
            None,
            Some(&mut c),
        )?;
        let d: ID3D11Device = d.context("нет устройства D3D11")?;
        let _ = d.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        Ok((d, c.context("нет контекста D3D11")?))
    }
}

/// Сохранить отрезок [start; end) в другом качестве: видео перекодируется на видеокарте
/// (начало — точно по кадру), звук копируется без изменений, если громкость дорожек не менялась.
#[allow(clippy::too_many_arguments)]
pub fn transcode(
    src: &Path,
    dst: &Path,
    start: f64,
    end: f64,
    audio_keep: Option<&[usize]>,
    audio_gains: Option<&[f32]>,
    q: &Quality,
    progress: &mut dyn FnMut(f64),
) -> Result<TrimResult> {
    if end <= start {
        bail!("конец отрезка раньше начала");
    }
    let info = mux::clip_info(src)?;
    if start >= info.duration {
        bail!("отрезок за пределами клипа");
    }
    let end = end.min(info.duration);
    let fps = if info.fps > 1.0 { info.fps } else { 60.0 };
    let (out_w, out_h) = q.out_size(info.width, info.height);
    let bitrate = q.bitrate(out_w, out_h, fps);

    let _mf = MfScope::new()?;
    let (dev, ctx) = device()?;
    let mut packets: Vec<Arc<Packet>> = Vec::new();
    let (first_ts, last_end) = unsafe {
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
        let reader = MFCreateSourceReaderFromURL(PCWSTR(wide(src).as_ptr()), &attrs).context("клип не открылся")?;
        reader.SetStreamSelection(ALL_STREAMS, false)?;
        reader.SetStreamSelection(FIRST_VIDEO, true)?;
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        reader.SetCurrentMediaType(FIRST_VIDEO, None, &t).context("декодер не выдаёт NV12")?;

        let start_ts = (start * 1e7) as i64;
        let end_ts = (end * 1e7) as i64;
        let frame_dur = (1e7 / fps) as i64;
        // PROPVARIANT с типом VT_I8 (время в 100 нс) — позиция для перемотки
        #[repr(C)]
        struct I8Variant {
            vt: u16,
            r: [u16; 3],
            value: i64,
            pad: i64,
        }
        let pos = I8Variant { vt: 20, r: [0; 3], value: start_ts, pad: 0 };
        reader.SetCurrentPosition(&GUID::zeroed(), &pos as *const I8Variant as *const _)?;

        let mut enc = Encoder::new(&dev, &EncoderSettings { width: out_w, height: out_h, fps: fps.round() as u32, bitrate })?;
        let conv = Converter::for_video(&dev, &ctx, info.width, info.height, out_w, out_h, fps.round() as u32)?;
        let pool = (0..8).map(|_| conv.make_target(&dev)).collect::<Result<Vec<_>>>()?;
        let mut pi = 0;
        let mut first: Option<i64> = None;
        let mut last = 0i64;
        let mut sink = |p: Packet| packets.push(Arc::new(p));

        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            reader.ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample))?;
            if flags & EOS != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            // перемотка встаёт на ключевой кадр раньше — кадры до начала пропускаем
            if ts + frame_dur / 2 < start_ts {
                continue;
            }
            if ts >= end_ts {
                break;
            }
            let buf = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer = buf.cast().context("декодер работает не на видеокарте")?;
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
            let tex = ID3D11Texture2D::from_raw(raw);
            let slice = dxgi.GetSubresourceIndex()?;
            let base = *first.get_or_insert(ts);

            enc.wait_ready(&mut sink)?;
            let (target, view) = &pool[pi];
            conv.convert_texture(&tex, slice, view)?;
            enc.push_at(target, ts - base, frame_dur)?;
            pi = (pi + 1) % pool.len();
            last = ts + frame_dur;
            progress(((ts - start_ts) as f64 / (end_ts - start_ts).max(1) as f64).clamp(0.0, 0.99));
        }
        enc.finish(&mut sink)?;
        drop(reader);
        (first.context("в отрезке нет кадров")?, last)
    };
    packets.sort_by_key(|p| p.pts);
    if !packets.first().is_some_and(|p| p.key) {
        bail!("кодировщик не начал с ключевого кадра");
    }

    // звук: те же кадры AAC из исходника, сдвинутые к началу отрезка (или с другой громкостью)
    let (tracks, names) = audio_segment(src, first_ts as f64 / 1e7, last_end as f64 / 1e7, audio_keep, audio_gains)?;
    let silence = if tracks.is_empty() { Vec::new() } else { crate::aac::silent_frame()? };
    let inputs: Vec<mux::AudioInput> = tracks.iter().map(|t| mux::AudioInput { packets: t, silence: &silence }).collect();
    mux::write_av_named(dst, &packets, &inputs, &names, out_w, out_h)?;
    progress(1.0);
    Ok(TrimResult { start: first_ts as f64 / 1e7, end: last_end as f64 / 1e7 })
}

/// Звук из AAC-пакетов файла — для перекодирования (данные не меняются).
pub(crate) fn packet(data: Vec<u8>, pos: i64) -> Arc<AudioPacket> {
    Arc::new(AudioPacket { data, pos })
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    /// Щелчок через кодировщик и декодер AAC Windows: где он оказался на выходе.
    #[test]
    fn aac_round_trip_delay() {
        let _mf = MfScope::new().unwrap();
        let mut enc = crate::aac::AacEncoder::new().unwrap();
        let n = 48_000usize;
        let mut pcm = vec![0f32; n * 2];
        for k in 0..64 {
            pcm[(10_000 + k) * 2] = 0.8;
            pcm[(10_000 + k) * 2 + 1] = 0.8;
        }
        let mut packets = Vec::new();
        enc.encode(0, &pcm, &mut packets).unwrap();
        let mut dec = unsafe { AacDecoder::new().unwrap() };
        let mut out = Vec::new();
        for p in &packets {
            unsafe { dec.decode_pcm(&p.data, &mut out).unwrap() };
        }
        let peak = out.chunks(2).position(|f| f[0].abs() > 0.3).expect("щелчок потерялся");
        println!("первый пакет pos={}, пакетов {}, отсчётов на выходе {}, щелчок на {}", packets[0].pos, packets.len(), out.len() / 2, peak);
        assert!((peak as i64 - 10_000 - DECODE_DELAY).abs() < 64, "щелчок на {peak}, ждали {}", 10_000 + DECODE_DELAY);
    }
}

#[cfg(test)]
mod size_tests {
    use super::Quality;

    #[test]
    fn fits_size_and_picks_resolution_by_bitrate() {
        // 30 с в 20 МБ: видео ≈ 4,7 Мбит/с — 1080p
        let q = Quality::for_size(20_000_000, 30.0, 1, 1.0).unwrap();
        assert_eq!(q.max_height, 1080);
        let total = (q.bitrate(1920, 1080, 60.0) as f64 + 160_000.0) * 30.0 / 8.0;
        assert!(total <= 20_000_000.0 * 0.93, "{total}");
        // минута в 10 МБ — 540p, две минуты — 480p
        assert_eq!(Quality::for_size(10_000_000, 60.0, 1, 1.0).unwrap().max_height, 540);
        assert_eq!(Quality::for_size(10_000_000, 120.0, 1, 1.0).unwrap().max_height, 480);
        // повторная попытка ужимает битрейт
        let a = Quality::for_size(10_000_000, 60.0, 1, 1.0).unwrap().fixed_bitrate.unwrap();
        let b = Quality::for_size(10_000_000, 60.0, 1, 0.8).unwrap().fixed_bitrate.unwrap();
        assert!(b < a);
    }

    #[test]
    fn too_long_for_limit_is_an_error_with_hint() {
        let e = Quality::for_size(10_000_000, 600.0, 1, 1.0).unwrap_err().to_string();
        assert!(e.contains("10 МБ") && e.contains("с хорошего видео"), "{e}");
    }
}
