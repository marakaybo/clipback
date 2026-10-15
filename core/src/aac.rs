//! AAC-кодировщик Windows (Media Foundation). 48 кГц, стерео, 160 кбит/с.

use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

pub const FRAME: i64 = 1024; // отсчётов в одном кадре AAC
pub const BITRATE: u32 = 160_000;

/// Готовый кадр AAC. Время — в отсчётах 48 кГц на общей шкале.
pub struct AudioPacket {
    pub data: Vec<u8>,
    pub pos: i64,
}

pub struct AacEncoder {
    mft: IMFTransform,
    out_size: u32,
    out_frames: i64,
    /// Позиция первого поданного отсчёта (кадры на выходе считаем от неё).
    base: Option<i64>,
}

impl AacEncoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER).context("нет AAC-кодировщика Windows")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, crate::audio::RATE)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, crate::audio::RATE * 4)?;

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, crate::audio::RATE)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, BITRATE / 8)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // «сырой» AAC, как нужно для MP4
            output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;

            mft.SetInputType(0, &input, 0).context("AAC: вход не принят")?;
            mft.SetOutputType(0, &output, 0).context("AAC: выход не принят")?;
            let info = mft.GetOutputStreamInfo(0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { mft, out_size: info.cbSize.max(8192), out_frames: 0, base: None })
        }
    }

    /// Подать стерео f32 (позиция первого кадра — `pos`), забрать готовые кадры AAC.
    pub fn encode(&mut self, pos: i64, samples: &[f32], out: &mut Vec<AudioPacket>) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let base = *self.base.get_or_insert(pos);
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())
            .collect();
        unsafe {
            let buf = MFCreateMemoryBuffer(pcm.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), p, pcm.len());
            buf.Unlock()?;
            buf.SetCurrentLength(pcm.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            let frames = samples.len() as i64 / 2;
            s.SetSampleTime(pos * 10_000_000 / crate::audio::RATE as i64)?;
            s.SetSampleDuration(frames * 10_000_000 / crate::audio::RATE as i64)?;
            self.mft.ProcessInput(0, &s, 0)?;
        }
        self.drain(base, out)
    }

    fn drain(&mut self, base: i64, out: &mut Vec<AudioPacket>) -> Result<()> {
        loop {
            unsafe {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                let mut db = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(Some(s)), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
                let mut status = 0;
                let r = self.mft.ProcessOutput(0, &mut db, &mut status);
                let sample = ManuallyDrop::take(&mut db[0].pSample);
                drop(ManuallyDrop::take(&mut db[0].pEvents));
                match r {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
                let Some(sample) = sample else { continue };
                let b = sample.ConvertToContiguousBuffer()?;
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                b.Lock(&mut p, None, Some(&mut len))?;
                let data = std::slice::from_raw_parts(p, len as usize).to_vec();
                b.Unlock()?;
                if data.is_empty() {
                    continue;
                }
                // кадры AAC идут подряд по 1024 отсчёта; задержку кодировщика компенсирует
                // то, что первый кадр соответствует первым поданным отсчётам
                out.push(AudioPacket { data, pos: base + self.out_frames * FRAME });
                self.out_frames += 1;
            }
        }
    }
}

/// Один кадр AAC с тишиной — им заполняются пропуски при сборке MP4.
pub fn silent_frame() -> Result<Vec<u8>> {
    let mut enc = AacEncoder::new()?;
    let mut out = Vec::new();
    enc.encode(0, &vec![0.0; FRAME as usize * 2 * 4], &mut out)?;
    out.into_iter().nth(1).or(None).map(|p| p.data).context("кодировщик не выдал кадр тишины")
}
