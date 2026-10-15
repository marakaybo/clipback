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
struct MfScope;
impl MfScope {
    fn new() -> Result<Self> {
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

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain([0]).collect()
}

const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const EOS: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;

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

    /// Раскодировать кадр; для каждого полученного стерео-кадра вызывается `f(номер, пик)`.
    unsafe fn decode(&mut self, data: &[u8], f: &mut dyn FnMut(f32)) -> Result<()> {
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
            let pcm = std::slice::from_raw_parts(q as *const i16, len as usize / 2);
            for fr in pcm.chunks(2) {
                f(fr.iter().map(|&x| (x as f32 / 32768.0).abs()).fold(0f32, f32::max));
            }
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
fn device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
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
/// (начало — точно по кадру), звук копируется без изменений.
pub fn transcode(
    src: &Path,
    dst: &Path,
    start: f64,
    end: f64,
    audio_keep: Option<&[usize]>,
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

    // звук: те же кадры AAC из исходника, сдвинутые к началу отрезка
    let (tracks, names) = mux::read_audio_segment(src, first_ts as f64 / 1e7, last_end as f64 / 1e7, audio_keep)?;
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
