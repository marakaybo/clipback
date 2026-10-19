//! Монтаж из плеера: оставленные куски клипа одним файлом «<имя> (монтаж).mp4» рядом с оригиналом.

use crate::{blocking, checked_path, clips, clips_changed, inherit_hidden, log, storage, Res};
use clipback_core::media::Quality;
use clipback_core::{montage, mux};
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter};

/// «Отмена» в интерфейсе: сохранение проверяет флаг между кадрами.
static CANCEL: AtomicBool = AtomicBool::new(false);

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
    quality: Option<String>,
) -> Res<MontageOut> {
    CANCEL.store(false, Ordering::SeqCst);
    blocking(move || run(app, path, pieces, audio_keep, audio_gains, quality)).await
}

#[tauri::command]
pub fn cancel_montage() {
    CANCEL.store(true, Ordering::SeqCst);
}

fn run(app: AppHandle, path: String, pieces: Vec<(f64, f64)>, audio_keep: Option<Vec<usize>>, audio_gains: Option<Vec<f32>>, quality: Option<String>) -> Res<MontageOut> {
    let p = checked_path(&app, &path)?;
    let q = quality.filter(|q| q != "original");
    let len: f64 = pieces.iter().map(|(s, e)| (e - s).max(0.0)).sum();
    storage::ensure_space(&app, storage::trim_expected_for(&p, 0.0, len, q.as_deref()), "монтаж не сохранён")?;
    let dst = montage::out_path(&p).map_err(|e| format!("{e:#}"))?;
    let progress_app = app.clone();
    let mut last = 0.0;
    let mut progress = move |f: f64| {
        if f - last >= 0.02 || f >= 1.0 {
            last = f;
            let _ = progress_app.emit("export-progress", f);
        }
    };
    let r = save(&p, &dst, &pieces, audio_keep.as_deref(), audio_gains.as_deref(), q.as_deref(), &mut progress).map_err(|e| {
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
    audio_keep: Option<&[usize]>,
    audio_gains: Option<&[f32]>,
    quality: Option<&str>,
    progress: &mut dyn FnMut(f64),
) -> anyhow::Result<montage::MontageOut> {
    // «discord:20» — уложить в 20 МБ; звук — одна дорожка, как у отрезка под размер
    if let Some(mb) = quality.and_then(|q| q.strip_prefix("discord:")).and_then(|v| v.parse::<u64>().ok()) {
        let limit = mb.clamp(1, 10_000) * 1_000_000;
        let info = mux::clip_info(p)?;
        let keep: Vec<usize> = match audio_keep {
            Some(k) => k.iter().copied().take(1).collect(),
            None => (info.audio_tracks > 0).then_some(0).into_iter().collect(),
        };
        let secs: f64 = pieces.iter().map(|(s, e)| (e.min(info.duration) - s).max(0.0)).sum();
        let mut squeeze = 1.0;
        for attempt in 0..3 {
            let q = Quality::for_size(limit, secs, keep.len(), squeeze)?;
            let r = montage::montage(p, dst, pieces, Some(&keep), audio_gains, &q, progress, &CANCEL)?;
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
    montage::montage(p, dst, pieces, audio_keep, audio_gains, &q, progress, &CANCEL)
}
