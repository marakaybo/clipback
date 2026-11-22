//! Монтаж из плеера: оставленные куски клипа одним файлом «<имя> (монтаж).mp4» рядом с оригиналом.

use crate::{blocking, checked_path, clips, clips_changed, inherit_hidden, log, storage, Res};
use clipback_core::media::{ExportAudio, Quality};
use clipback_core::{montage, mux};
use serde::Serialize;
use std::path::Path;
use crate::export::CANCEL;
use std::sync::atomic::Ordering;
use tauri::AppHandle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MontageOut {
    clip: clips::ClipEntry,
    duration: f64,
    pieces: usize,
}

#[tauri::command]
pub async fn montage_clip(
    app: AppHandle,
    path: String,
    pieces: Vec<(f64, f64)>,
    audio_keep: Option<Vec<usize>>,
    audio_gains: Option<Vec<f32>>,
    mixdown: Option<bool>,
    quality: Option<String>,
) -> Res<MontageOut> {
    CANCEL.store(false, Ordering::SeqCst);
    blocking(move || {
        let audio = ExportAudio { keep: audio_keep.as_deref(), gains: audio_gains.as_deref(), mixdown: mixdown.unwrap_or(false) };
        run(app, path, pieces, audio, quality)
    }).await
}

#[tauri::command]
pub fn cancel_montage() {
    crate::export::cancel_export();
}

fn run(app: AppHandle, path: String, pieces: Vec<(f64, f64)>, audio: ExportAudio<'_>, quality: Option<String>) -> Res<MontageOut> {
    let p = checked_path(&app, &path)?;
    let q = quality.filter(|q| q != "original");
    let len: f64 = pieces.iter().map(|(s, e)| (e - s).max(0.0)).sum();
    storage::ensure_space(&app, storage::trim_expected_for(&p, 0.0, len, q.as_deref()), "монтаж не сохранён")?;
    let dst = montage::out_path(&p).map_err(|e| format!("{e:#}"))?;
    let mut progress = crate::export::progress(&app);
    let r = save(&p, &dst, &pieces, audio, q.as_deref(), &mut progress).map_err(|e| {
        let e = format!("{e:#}");
        if e != "отменено" {
            log(format!("не удалось сохранить монтаж: {e}"));
        }
        e
    })?;
    let mut clip = clips::entry(&dst).map_err(|e| format!("{e:#}"))?;
    inherit_hidden(&app, &p, &mut clip);
    log(format!("монтаж: {} кусков, {:.2} с (качество {}): {}", r.pieces, r.duration, q.as_deref().unwrap_or("оригинал"), clip.path));
    clips_changed(&app);
    Ok(MontageOut { clip, duration: r.duration, pieces: r.pieces })
}

fn save(
    p: &Path,
    dst: &Path,
    pieces: &[(f64, f64)],
    audio: ExportAudio<'_>,
    quality: Option<&str>,
    progress: &mut dyn FnMut(f64),
) -> anyhow::Result<montage::MontageOut> {
    // «discord:20» — уложить в 20 МБ; звук — одна дорожка, как у отрезка под размер
    if let Some(mb) = quality.and_then(|q| q.strip_prefix("discord:")).and_then(|v| v.parse::<u64>().ok()) {
        let limit = mb.clamp(1, 10_000) * 1_000_000;
        let info = mux::clip_info(p)?;
        let audio = ExportAudio { mixdown: true, ..audio };
        let tracks = audio.track_count(&info.audio_names);
        let secs: f64 = pieces.iter().map(|(s, e)| (e.min(info.duration) - s).max(0.0)).sum();
        let mut squeeze = 1.0;
        for attempt in 0..3 {
            let q = Quality::for_size(limit, secs, tracks, squeeze)?;
            let r = montage::montage_with_audio(p, dst, pieces, audio, &q, progress, &CANCEL)?;
            let size = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
            if size <= limit || attempt == 2 {
                return Ok(r);
            }
            squeeze *= limit as f64 / size as f64 * 0.95;
        }
        unreachable!()
    }
    let q = match quality.and_then(Quality::preset) {
        Some(q) => q,
        None => montage::source_quality(p)?,
    };
    montage::montage_with_audio(p, dst, pieces, audio, &q, progress, &CANCEL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Two independent stereo AAC tracks: microphone on the left, app on the right.
    /// Decode the actual MP4 from each export and measure both channels across the cut.
    #[test]
    #[ignore = "requires ffmpeg and the Windows hardware video encoder"]
    fn mixdown_audio_outputs() {
        let dir = std::env::temp_dir().join(format!("clipback-mixdown-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("source.mp4");
        let fixture = Command::new("ffmpeg").args([
            "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=3",
            "-f", "lavfi", "-i", "aevalsrc=0.2*sin(440*2*PI*t)|0:s=48000:d=3",
            "-f", "lavfi", "-i", "aevalsrc=0|0.2*sin(660*2*PI*t):s=48000:d=3",
            "-map", "0:v", "-map", "1:a", "-map", "2:a", "-c:v", "libx264", "-bf", "0", "-g", "30", "-c:a", "aac", "-b:a", "192k",
        ]).arg(&src).output().unwrap();
        assert!(fixture.status.success(), "{}", String::from_utf8_lossy(&fixture.stderr));
        mux::write_track_names(&src, &["Микрофон".into(), "Discord".into()]).unwrap();

        let audio = ExportAudio { keep: Some(&[0, 1]), gains: Some(&[0.5, 0.25]), mixdown: true };
        let pieces = [(0.0, 1.0), (2.0, 3.0)];
        for quality in [None, Some("low"), Some("discord:1")] {
            // Discord must collapse tracks even when the switch is off.
            let selection = ExportAudio { mixdown: quality != Some("discord:1"), ..audio };
            let trim = clips::trim(&src, 1.0, 3.0, false, selection, quality, &mut |_| {}, &CANCEL).unwrap();
            let montage = dir.join("montage.mp4");
            save(&src, &montage, &pieces, selection, quality, &mut |_| {}).unwrap();
            for path in [Path::new(&trim.clip.path), &montage] {
                let info = mux::clip_info(path).unwrap();
                assert_eq!(info.audio_tracks, 1);
                let decoded = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path)
                    .args(["-map", "0:a:0", "-f", "f32le", "pipe:1"]).output().unwrap();
                assert!(decoded.status.success(), "{}", String::from_utf8_lossy(&decoded.stderr));
                for offset in [24_000, 72_000] {
                    let frames = &decoded.stdout[offset * 8..(offset + 12_000) * 8];
                    for (channel, gain) in [0.5, 0.25].into_iter().enumerate() {
                        let power: f32 = frames.chunks_exact(8).map(|f| {
                            f32::from_le_bytes(f[channel * 4..channel * 4 + 4].try_into().unwrap()).powi(2)
                        }).sum();
                        let rms = (power / 12_000.0).sqrt();
                        let expected = 0.2 * gain * std::f32::consts::FRAC_1_SQRT_2;
                        assert!((rms - expected).abs() < 0.006, "{path:?} {quality:?} ch={channel} rms={rms}, expected {expected}");
                    }
                }
            }
        }
        let silent = ExportAudio { keep: Some(&[]), ..audio };
        let trim = clips::trim(&src, 0.0, 1.0, false, silent, Some("discord:1"), &mut |_| {}, &CANCEL).unwrap();
        assert_eq!(trim.clip.audio_tracks, 0);
        let montage = dir.join("muted.mp4");
        save(&src, &montage, &pieces, silent, Some("discord:1"), &mut |_| {}).unwrap();
        assert_eq!(mux::clip_info(&montage).unwrap().audio_tracks, 0);
        save(&src, &montage, &pieces, ExportAudio::default(), None, &mut |_| {}).unwrap();
        assert_eq!(mux::clip_info(&montage).unwrap().audio_names, ["Микрофон", "Discord"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
