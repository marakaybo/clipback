//! Короткие тихие звуки: «сохранено», «запись началась», «запись остановлена».
//! Синтезируются в памяти при первом вызове (WAV, ~4 КБ) и играются системой без
//! отдельных файлов и без лишних библиотек.

use std::sync::OnceLock;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

#[derive(Clone, Copy)]
pub enum Cue {
    Saved,
    RecordStart,
    RecordStop,
}

const RATE: u32 = 44_100;

/// Мягкий щелчок: синусоида с быстрым затуханием (без резкой атаки — не «пищит»).
fn tone(freqs: &[(f32, f32)], gap_ms: f32) -> Vec<i16> {
    let mut out = Vec::new();
    for (i, &(f, ms)) in freqs.iter().enumerate() {
        if i > 0 {
            out.extend(std::iter::repeat(0).take((RATE as f32 * gap_ms / 1000.0) as usize));
        }
        let n = (RATE as f32 * ms / 1000.0) as usize;
        for k in 0..n {
            let t = k as f32 / RATE as f32;
            let attack = (k as f32 / (RATE as f32 * 0.003)).min(1.0);
            let decay = (-t * 1000.0 / (ms * 0.35)).exp();
            // основной тон + октавой ниже для «тела» звука
            let s = (std::f32::consts::TAU * f * t).sin() * 0.75 + (std::f32::consts::TAU * f * 0.5 * t).sin() * 0.25;
            out.push((s * attack * decay * 0.22 * i16::MAX as f32) as i16);
        }
    }
    out
}

fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // моно
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

fn data(cue: Cue) -> &'static [u8] {
    static SAVED: OnceLock<Vec<u8>> = OnceLock::new();
    static START: OnceLock<Vec<u8>> = OnceLock::new();
    static STOP: OnceLock<Vec<u8>> = OnceLock::new();
    match cue {
        Cue::Saved => SAVED.get_or_init(|| wav(&tone(&[(1320.0, 70.0)], 0.0))),
        Cue::RecordStart => START.get_or_init(|| wav(&tone(&[(880.0, 60.0), (1320.0, 80.0)], 25.0))),
        Cue::RecordStop => STOP.get_or_init(|| wav(&tone(&[(1320.0, 60.0), (880.0, 80.0)], 25.0))),
    }
}

/// Проиграть звук, не дожидаясь конца. Данные статические — живут, пока играет система.
pub fn play(cue: Cue) {
    let d = data(cue);
    unsafe {
        let _ = PlaySoundW(PCWSTR(d.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_are_short_quiet_wavs() {
        for cue in [Cue::Saved, Cue::RecordStart, Cue::RecordStop] {
            let d = data(cue);
            assert_eq!(&d[..4], b"RIFF");
            assert_eq!(&d[8..12], b"WAVE");
            let samples: Vec<i16> = d[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            let secs = samples.len() as f32 / RATE as f32;
            assert!(secs > 0.05 && secs < 0.25, "{secs}");
            let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
            assert!(peak < i16::MAX as u16 / 3, "слишком громко: {peak}");
            // начало и конец без щелчка
            assert!(samples[0].unsigned_abs() < 500 && samples.last().unwrap().unsigned_abs() < 500);
        }
    }
}
