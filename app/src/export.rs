//! Файлы для отправки: копия клипа с одной звуковой дорожкой. Плееры, Discord и мессенджеры
//! играют только первую дорожку, поэтому «Копировать» и перетаскивание в другие программы
//! отдают такую копию (из временной папки), а «Одна дорожка» сохраняет её рядом с оригиналом.
//! Здесь же общая «Отмена» долгих сохранений и ход работы для интерфейса.

use crate::{blocking, checked_path, clips, clips_changed, inherit_hidden, log, out_dir, storage, winutil, Res};
use clipback_core::{mux, util};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter};

/// «Отмена» в интерфейсе: сохранение проверяет флаг между кадрами.
pub static CANCEL: AtomicBool = AtomicBool::new(false);

#[tauri::command]
pub fn cancel_export() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// Ход работы для интерфейса: событие `export-progress` (0…1), не чаще чем на 1 %.
pub fn progress(app: &AppHandle) -> impl FnMut(f64) {
    let app = app.clone();
    let mut last = -1.0;
    move |f: f64| {
        if f - last >= 0.01 || (f >= 1.0 && last < 1.0) {
            last = f;
            let _ = app.emit("export-progress", f);
        }
    }
}

fn needs_one_track(count: usize, keep: Option<&[usize]>, gains: Option<&[f32]>) -> bool {
    count > 1 || (count == 1 && (keep.is_some_and(|k| !k.contains(&0)) || gains.and_then(|g| g.first()).is_some_and(|&g| g != 1.0)))
}

// ---------- «Одна дорожка» рядом с оригиналом ----------

/// «<имя> (одна дорожка).mp4» рядом с каждым клипом. Ход работы — общий на все клипы.
#[tauri::command]
pub async fn one_track_clips(app: AppHandle, paths: Vec<String>, audio_keep: Option<Vec<usize>>, audio_gains: Option<Vec<f32>>) -> Res<Vec<clips::ClipEntry>> {
    CANCEL.store(false, Ordering::SeqCst);
    blocking(move || {
        let n = paths.len().max(1) as f64;
        let mut emit = progress(&app);
        let mut out = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let p = checked_path(&app, path)?;
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            storage::ensure_space(&app, size, "копия с одной дорожкой не сохранена")?;
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = p.parent().ok_or("нет папки")?;
            let dst = util::unique_path(dir, &format!("{stem} (одна дорожка)"), "mp4");
            mux::one_track_selected(&p, &dst, audio_keep.as_deref(), audio_gains.as_deref(), &mut |f| emit((i as f64 + f) / n), &CANCEL).map_err(|e| {
                let e = format!("{e:#}");
                if e != "отменено" {
                    log(format!("одна дорожка: {e}"));
                }
                e
            })?;
            let mut clip = clips::entry(&dst).map_err(|e| format!("{e:#}"))?;
            inherit_hidden(&app, &p, &mut clip);
            log(format!("одна дорожка: {}", clip.path));
            out.push(clip);
        }
        clips_changed(&app);
        Ok(out)
    })
    .await
}

// ---------- копия для «Копировать» и перетаскивания ----------

/// Временные копии живут сутки: Discord и мессенджеры читают файл, когда его отправляют.
const SHARE_KEEP: Duration = Duration::from_secs(24 * 3600);
/// И не больше пяти — каждая размером с клип.
const SHARE_MAX: usize = 5;
static SHARE_FILES: Mutex<()> = Mutex::new(());

pub fn share_dir() -> PathBuf {
    std::env::temp_dir().join("Clipback").join("share")
}

/// Папка копии: своя для каждого клипа (по пути, размеру, времени изменения) и громкости.
fn share_key(p: &Path, keep: Option<&[usize]>, gains: Option<&[f32]>) -> String {
    let mut h = std::hash::DefaultHasher::new();
    p.hash(&mut h);
    if let Ok(m) = std::fs::metadata(p) {
        m.len().hash(&mut h);
        m.modified().ok().hash(&mut h);
    }
    keep.hash(&mut h);
    gains.map(|g| g.iter().map(|x| x.to_bits()).collect::<Vec<_>>()).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Готовая копия: в своей папке, с именем оригинала (так её и увидит получатель).
fn share_path(p: &Path, keep: Option<&[usize]>, gains: Option<&[f32]>) -> Option<PathBuf> {
    Some(share_dir().join(share_key(p, keep, gains)).join(p.file_name()?))
}

/// Убрать копии старше суток и лишние сверх пяти (новые остаются).
pub fn clean_share(keep: Option<&Path>) {
    let _guard = SHARE_FILES.lock().unwrap_or_else(|e| e.into_inner());
    clean_dir(&share_dir(), SHARE_MAX, keep);
}

/// Убрать из `root` папки старше суток и лишние сверх `max` (новые остаются).
fn clean_dir(root: &Path, max: usize, keep: Option<&Path>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let now = SystemTime::now();
    let mut dirs: Vec<(SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(now), e.path()))
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (k, (t, d)) in dirs.iter().enumerate() {
        let old = now.duration_since(*t).unwrap_or_default() > SHARE_KEEP;
        if (old || k >= max) && keep.is_none_or(|x| !x.starts_with(d)) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Путь, который отдать другой программе: копия с одной дорожкой (сделать, если её нет)
/// или сам клип, если звуковая дорожка у него одна.
fn share_file(app: &AppHandle, p: &Path, keep: Option<&[usize]>, gains: Option<&[f32]>) -> Res<PathBuf> {
    if !p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("mp4")) {
        return Ok(p.to_path_buf());
    }
    let info = match mux::clip_info(p) {
        Ok(info) => info,
        Err(_) if keep.is_none() && gains.is_none() => return Ok(p.to_path_buf()),
        Err(e) => return Err(format!("{e:#}")),
    };
    if !needs_one_track(info.audio_tracks, keep, gains) {
        return Ok(p.to_path_buf());
    }
    let _guard = SHARE_FILES.lock().unwrap_or_else(|e| e.into_inner());
    let dst = share_path(p, keep, gains).ok_or("нет имени файла")?;
    if dst.exists() {
        return Ok(dst);
    }
    let dir = dst.parent().ok_or("нет папки")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{e:#}"))?;
    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if !storage::enough_space(winutil::free_space(dir), size) {
        return Err(format!("Мало места на диске {} для копии", storage::drive_name(dir)));
    }
    CANCEL.store(false, Ordering::SeqCst);
    let t = std::time::Instant::now();
    let mut emit = progress(app);
    mux::one_track_selected(p, &dst, keep, gains, &mut emit, &CANCEL).map_err(|e| {
        let _ = std::fs::remove_dir_all(dir);
        format!("{e:#}")
    })?;
    log(format!("копия с одной дорожкой для отправки за {:.1} с: {}", t.elapsed().as_secs_f64(), dst.display()));
    clean_dir(&share_dir(), SHARE_MAX, Some(&dst));
    Ok(dst)
}

/// Подготовить файл для отправки; возвращает его путь (интерфейсу — чтобы знать, что готово).
#[tauri::command]
pub async fn share_copy(app: AppHandle, path: String, audio_keep: Option<Vec<usize>>, audio_gains: Option<Vec<f32>>) -> Res<String> {
    let p = checked_path(&app, &path)?;
    blocking(move || share_file(&app, &p, audio_keep.as_deref(), audio_gains.as_deref()).map(|d| d.to_string_lossy().into_owned())).await
}

/// «Копировать»: в буфер обмена кладётся файл с одной дорожкой.
#[tauri::command]
pub async fn copy_clip(app: AppHandle, path: String, audio_keep: Option<Vec<usize>>, audio_gains: Option<Vec<f32>>) -> Res<()> {
    let p = checked_path(&app, &path)?;
    blocking(move || {
        let f = share_file(&app, &p, audio_keep.as_deref(), audio_gains.as_deref())?;
        winutil::copy_file_to_clipboard(&f).map_err(|e| format!("{e:#}"))
    })
    .await
}

// ---------- дорожки для плеера ----------

/// Звуковые дорожки клипа отдельными файлами: `<video>` играет только первую, а плеер
/// должен звучать как сохранённый отрезок — с выключенными дорожками и их громкостью.
pub fn tracks_dir() -> PathBuf {
    std::env::temp_dir().join("Clipback").join("tracks")
}

/// Файлы .m4a дорожек `tracks` (номера в клипе), в том же порядке. Делаются один раз на
/// версию клипа (копирование без перекодирования — доли секунды), хранятся сутки.
#[tauri::command]
pub async fn preview_tracks(app: AppHandle, path: String, tracks: Vec<usize>) -> Res<Vec<String>> {
    let p = checked_path(&app, &path)?;
    blocking(move || {
        static PREVIEW_FILES: Mutex<()> = Mutex::new(());
        let _guard = PREVIEW_FILES.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tracks_dir().join(share_key(&p, None, None));
        let mut out = Vec::new();
        for i in tracks {
            let f = dir.join(format!("{i}.m4a"));
            if !f.exists() {
                mux::extract_audio(&p, &f, i).map_err(|e| format!("{e:#}"))?;
            }
            out.push(f.to_string_lossy().into_owned());
        }
        clean_dir(&tracks_dir(), 8, Some(&dir));
        Ok(out)
    })
    .await
}

/// Что можно отдать перетаскиванием: клип из папки клипов или его готовая копия для отправки.
pub fn drag_path(app: &AppHandle, path: &str) -> Res<PathBuf> {
    let p = Path::new(path);
    if let Ok(inside) = clips::ensure_inside(&share_dir(), p) {
        if inside.is_file() {
            return Ok(inside);
        }
    }
    clips::ensure_inside(&out_dir(app), p).map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_copy_is_keyed_by_clip_selection_and_volumes() {
        let dir = std::env::temp_dir().join("clipback-share-test");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.mp4");
        std::fs::write(&a, b"one").unwrap();
        let k1 = share_key(&a, None, None);
        assert_eq!(k1, share_key(&a, None, None), "та же копия — без повторной работы");
        assert_ne!(k1, share_key(&a, None, Some(&[1.0, 1.0, 0.5])), "другая громкость — другая копия");
        assert_ne!(k1, share_key(&a, Some(&[]), None));
        assert_ne!(share_key(&a, Some(&[0]), None), share_key(&a, Some(&[1]), None));
        std::fs::write(&a, b"changed").unwrap();
        assert_ne!(k1, share_key(&a, None, None), "клип изменился (обрезали) — копия заново");
        let p = share_path(&a, None, None).unwrap();
        assert_eq!(p.file_name().unwrap(), "a.mp4", "получатель видит имя оригинала");
        assert!(p.starts_with(share_dir()));
    }

    #[test]
    fn share_single_track_respects_selection_and_gain() {
        assert!(!needs_one_track(1, None, None));
        assert!(!needs_one_track(1, Some(&[0]), Some(&[1.0])));
        assert!(needs_one_track(1, Some(&[]), None));
        assert!(needs_one_track(1, None, Some(&[0.5])));
        assert!(needs_one_track(1, None, Some(&[0.0])));
    }
}
