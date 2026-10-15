//! Аппаратный H.264 через Media Foundation (у NVIDIA это NVENC, у AMD — AMF, у Intel — QuickSync).
//! Кадры подаются прямо D3D11-текстурами, в процессор ничего не копируется.

use anyhow::{bail, Context, Result};
use std::mem::ManuallyDrop;
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

pub struct Packet {
    /// H.264 в формате Annex-B (с 00 00 00 01 перед каждым NAL).
    pub data: Vec<u8>,
    /// Время в единицах 100 нс.
    pub pts: i64,
    pub key: bool,
}

pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

pub struct Encoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    _manager: IMFDXGIDeviceManager,
    pub name: String,
    need_input: u32,
    provides_samples: bool,
    out_buf_size: u32,
    frame_dur: i64,
}

fn set_codec(api: &ICodecAPI, key: &GUID, v: VARIANT, what: &str) {
    if let Err(e) = unsafe { api.SetValue(key, &v) } {
        crate::util::log(format!("кодировщик: не удалось задать {what}: {e}"));
    }
}

impl Encoder {
    pub fn new(device: &ID3D11Device, s: &EncoderSettings) -> Result<Self> {
        unsafe {
            let (mft, name) = find_hardware_h264().context("не найден аппаратный кодировщик H.264")?;

            let attrs = mft.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            // передаём кодировщику наше D3D-устройство, чтобы он брал кадры прямо с видеокарты
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("кодировщик не принимает D3D11-устройство")?;

            if let Ok(api) = mft.cast::<ICodecAPI>() {
                // 2 = переменный битрейт без жёсткого потолка: качество стабильнее
                set_codec(&api, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(2u32), "режим битрейта");
                set_codec(&api, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(s.bitrate), "битрейт");
                // ключевой кадр каждую секунду — отрезок можно начать почти с любого места
                set_codec(&api, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(s.fps), "интервал ключевых кадров");
                // без B-кадров: проще и надёжнее резать без перекодирования
                // NVENC через MF и так не делает B-кадры и этот параметр не принимает
                let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                set_codec(&api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true), "низкую задержку");
            }

            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, ((s.width as u64) << 32) | s.height as u64)?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, ((s.fps as u64) << 32) | 1)?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            set_bt709(&out_type);
            mft.SetOutputType(0, &out_type, 0).context("кодировщик не принял параметры выхода")?;

            let mut in_type = None;
            for i in 0.. {
                let t = match mft.GetInputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(_) => break,
                };
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    in_type = Some(t);
                    break;
                }
            }
            let in_type = in_type.context("кодировщик не принимает NV12")?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, ((s.width as u64) << 32) | s.height as u64)?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, ((s.fps as u64) << 32) | 1)?;
            set_bt709(&in_type);
            mft.SetInputType(0, &in_type, 0).context("кодировщик не принял параметры входа")?;

            let info = mft.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let events: IMFMediaEventGenerator = mft.cast()?;
            Ok(Self {
                mft,
                events,
                _manager: manager,
                name,
                need_input: 0,
                provides_samples,
                out_buf_size: info.cbSize.max(s.width * s.height),
                frame_dur: 10_000_000 / s.fps as i64,
            })
        }
    }

    /// Разобрать накопившиеся события кодировщика, не блокируясь.
    /// Готовые пакеты отдаются в `on_packet`.
    pub fn poll(&mut self, on_packet: &mut dyn FnMut(Packet)) -> Result<()> {
        loop {
            let ev = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            let ty = unsafe { ev.GetType()? };
            if ty == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if ty == METransformHaveOutput.0 as u32 {
                if let Some(p) = self.pull_output()? {
                    on_packet(p);
                }
            }
        }
    }

    /// Готов ли кодировщик принять ещё один кадр.
    pub fn wants_input(&self) -> bool {
        self.need_input > 0
    }

    pub fn push(&mut self, tex: &ID3D11Texture2D, frame_index: i64) -> Result<()> {
        self.push_at(tex, frame_index * self.frame_dur, self.frame_dur)
    }

    /// Подать кадр с явным временем (единицы 100 нс).
    pub fn push_at(&mut self, tex: &ID3D11Texture2D, pts: i64, dur: i64) -> Result<()> {
        if self.need_input == 0 {
            bail!("кодировщик не готов");
        }
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(dur)?;
            self.mft.ProcessInput(0, &sample, 0)?;
        }
        self.need_input -= 1;
        Ok(())
    }

    /// Дождаться, пока кодировщик попросит кадр (готовые пакеты отдаются в `on_packet`).
    pub fn wait_ready(&mut self, on_packet: &mut dyn FnMut(Packet)) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            self.poll(on_packet)?;
            if self.wants_input() {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                bail!("кодировщик перестал отвечать");
            }
            std::thread::sleep(std::time::Duration::from_micros(300));
        }
    }

    /// Закончить поток: забрать все оставшиеся кадры.
    pub fn finish(&mut self, on_packet: &mut dyn FnMut(Packet)) -> Result<()> {
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ev = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if std::time::Instant::now() > deadline {
                        bail!("кодировщик не закончил поток");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let ty = unsafe { ev.GetType()? };
            if ty == METransformHaveOutput.0 as u32 {
                if let Some(p) = self.pull_output()? {
                    on_packet(p);
                }
            } else if ty == METransformDrainComplete.0 as u32 {
                return Ok(());
            }
        }
    }

    fn pull_output(&mut self) -> Result<Option<Packet>> {
        unsafe {
            let own_sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_buf_size)?)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut out, &mut status);
            let sample = ManuallyDrop::take(&mut out[0].pSample);
            drop(ManuallyDrop::take(&mut out[0].pEvents));

            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // кодировщик поменял формат выхода — принимаем предложенный
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            let Some(sample) = sample else { return Ok(None) };

            let pts = sample.GetSampleTime().unwrap_or(0);
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buf.Unlock()?;
            Ok(Some(Packet { data, pts, key }))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.events.cast::<IMFShutdown>().map(|s| s.Shutdown());
        }
    }
}

/// Пометить поток как BT.709, ограниченный диапазон — так плееры не исказят цвета.
fn set_bt709(t: &IMFMediaType) {
    unsafe {
        let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    }
}

unsafe fn find_hardware_h264() -> Result<(IMFTransform, String)> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &mut list,
        &mut count,
    )?;
    if count == 0 || list.is_null() {
        bail!("аппаратных кодировщиков H.264 нет");
    }
    let activates: Vec<Option<IMFActivate>> = (0..count as usize).map(|i| std::ptr::read(list.add(i))).collect();
    CoTaskMemFree(Some(list as *const _));

    let mut last_err = None;
    for a in activates.into_iter().flatten() {
        let name = {
            let mut p = windows::core::PWSTR::null();
            let mut n = 0u32;
            if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut n).is_ok() {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            } else {
                "аппаратный H.264".into()
            }
        };
        match a.ActivateObject::<IMFTransform>() {
            Ok(t) => return Ok((t, name)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.map(Into::into).unwrap_or_else(|| anyhow::anyhow!("кодировщик не запустился")))
}
