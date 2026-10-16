//! Клипы и скриншоты в папке сохранения.

use anyhow::{bail, Context, Result};
use clipback_core::{mux, shot, util};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipEntry {
    pub path: String,
    pub name: String,
    /// "video" | "shot"
    pub kind: &'static str,
    pub size: u64,
    /// Время изменения, мс с 1970 года.
    pub modified: u64,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub audio_tracks: usize,
    /// Названия звуковых дорожек.
    pub audio_names: Vec<String>,
    /// Файл не читается (повреждён или не MP4).
    pub broken: bool,
    /// Отмечен звёздочкой: виден в фильтре «Избранное», автоочистка его не трогает.
    pub favorite: bool,
    /// Скрытая запись: в списке и плеере размыта, показывается только после подтверждения.
    pub hidden: bool,
}

fn kind_of(p: &Path) -> Option<&'static str> {
    match p.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "mp4" => Some("video"),
        "png" | "jpg" | "jpeg" => Some("shot"),
        _ => None,
    }
}

/// Размер PNG из заголовка (без чтения всей картинки).
/// Размер картинки по заголовку файла (PNG или JPEG), без чтения всего файла.
fn image_size(p: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut b = Vec::with_capacity(65536);
    std::fs::File::open(p).ok()?.take(65536).read_to_end(&mut b).ok()?;
    image_size_of(&b)
}

fn image_size_of(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() >= 24 && &b[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some((u32::from_be_bytes(b[16..20].try_into().ok()?), u32::from_be_bytes(b[20..24].try_into().ok()?)));
    }
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    // JPEG: идём по маркерам до SOF — там высота и ширина
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

pub fn entry(p: &Path) -> Result<ClipEntry> {
    let kind = kind_of(p).context("не клип")?;
    let meta = std::fs::metadata(p)?;
    let modified = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
    let mut e = ClipEntry {
        path: p.to_string_lossy().into_owned(),
        name: p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        kind,
        size: meta.len(),
        modified,
        duration: 0.0,
        width: 0,
        height: 0,
        fps: 0.0,
        audio_tracks: 0,
        audio_names: Vec::new(),
        broken: false,
        favorite: false,
        hidden: false,
    };
    if kind == "video" {
        match mux::clip_info(p) {
            Ok(i) => {
                e.duration = i.duration;
                e.width = i.width;
                e.height = i.height;
                e.fps = i.fps;
                e.audio_tracks = i.audio_tracks;
                e.audio_names = i.audio_names;
            }
            Err(_) => e.broken = true,
        }
    } else if let Some((w, h)) = image_size(p) {
        e.width = w;
        e.height = h;
    }
    Ok(e)
}

/// Все клипы в папке, новые сверху.
pub fn list(dir: &Path) -> Vec<ClipEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<ClipEntry> = rd
        .flatten()
        .map(|d| d.path())
        .filter(|p| p.is_file() && kind_of(p).is_some())
        .filter_map(|p| entry(&p).ok())
        .collect();
    v.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.name.cmp(&a.name)));
    v
}

/// canonicalize() в Windows даёт `\\?\C:\...` — возвращаем обычный вид `C:\...`,
/// иначе пути не совпадут с теми, что видит интерфейс.
fn simplify(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

/// Проверить, что путь лежит в папке клипов (команды из интерфейса не трогают другие файлы).
pub fn ensure_inside(dir: &Path, p: &Path) -> Result<PathBuf> {
    let dir = dir.canonicalize().context("папка клипов не найдена")?;
    let p = p.canonicalize().context("файл не найден")?;
    if !p.starts_with(&dir) || kind_of(&p).is_none() {
        bail!("файл не из папки клипов");
    }
    Ok(simplify(p))
}

/// Имя файла без запрещённых символов.
pub fn clean_name(name: &str) -> Result<String> {
    let s: String = name
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') && !c.is_control())
        .collect();
    let s = s.trim().trim_end_matches('.').trim().to_string();
    if s.is_empty() {
        bail!("пустое имя");
    }
    if s.chars().count() > 120 {
        bail!("слишком длинное имя");
    }
    let upper = s.to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2", "LPT3"];
    if reserved.contains(&upper.as_str()) {
        bail!("такое имя в Windows занято");
    }
    Ok(s)
}

pub fn rename(p: &Path, new_name: &str) -> Result<ClipEntry> {
    let name = clean_name(new_name)?;
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    let dst = p.with_file_name(format!("{name}.{ext}"));
    if dst == p {
        return entry(p);
    }
    if dst.exists() {
        bail!("файл с таким именем уже есть");
    }
    std::fs::rename(p, &dst).context("не удалось переименовать — файл может быть открыт в другой программе")?;
    entry(&dst)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrimOut {
    pub clip: ClipEntry,
    pub start: f64,
    pub end: f64,
}

/// Вырезать отрезок: копией рядом или вместо исходного файла.
/// `quality`: None — без перекодирования (мгновенно), иначе «high» | «medium» | «low».
#[allow(clippy::too_many_arguments)]
pub fn trim(
    p: &Path,
    start: f64,
    end: f64,
    replace: bool,
    audio_keep: Option<&[usize]>,
    audio_gains: Option<&[f32]>,
    quality: Option<&str>,
    progress: &mut dyn FnMut(f64),
) -> Result<TrimOut> {
    let dst = if replace {
        p.to_path_buf()
    } else {
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dir = p.parent().context("нет папки")?;
        util::unique_path(dir, &format!("{stem} (отрезок)"), "mp4")
    };
    // «discord:20» — уложить в 20 МБ
    if let Some(mb) = quality.and_then(|q| q.strip_prefix("discord:")).and_then(|v| v.parse::<u64>().ok()) {
        return trim_to_size(p, &dst, start, end, audio_keep, audio_gains, mb.clamp(1, 10_000) * 1_000_000, progress);
    }
    let r = match quality.and_then(clipback_core::media::Quality::preset) {
        Some(q) => clipback_core::media::transcode(p, &dst, start, end, audio_keep, audio_gains, &q, progress)?,
        None => mux::trim_tracks(p, &dst, start, end, audio_keep, audio_gains)?,
    };
    Ok(TrimOut { clip: entry(&dst)?, start: r.start, end: r.end })
}

/// Сохранить отрезок так, чтобы файл был не больше `limit` байт (для Discord и мессенджеров).
/// Звук — одна дорожка: Discord и плееры всё равно играют только первую, а место дорогое.
#[allow(clippy::too_many_arguments)]
fn trim_to_size(
    p: &Path,
    dst: &Path,
    start: f64,
    end: f64,
    audio_keep: Option<&[usize]>,
    audio_gains: Option<&[f32]>,
    limit: u64,
    progress: &mut dyn FnMut(f64),
) -> Result<TrimOut> {
    use clipback_core::media::{transcode, Quality};
    let info = mux::clip_info(p)?;
    let keep: Vec<usize> = match audio_keep {
        Some(k) => k.iter().copied().take(1).collect(),
        None => (info.audio_tracks > 0).then_some(0).into_iter().collect(),
    };
    let secs = end.min(info.duration) - start;
    let mut squeeze = 1.0;
    for attempt in 0..3 {
        let q = Quality::for_size(limit, secs, keep.len(), squeeze)?;
        let r = transcode(p, dst, start, end, Some(&keep), audio_gains, &q, progress)?;
        let size = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        if size <= limit || attempt == 2 {
            return Ok(TrimOut { clip: entry(dst)?, start: r.start, end: r.end });
        }
        // кодировщик превысил битрейт — ужимаем пропорционально и пробуем ещё раз
        squeeze *= limit as f64 / size as f64 * 0.95;
    }
    unreachable!()
}

/// Сохранить картинку (PNG или JPEG — формат определяется по данным).
pub fn save_image(dir: &Path, label: &str, data: &[u8]) -> Result<ClipEntry> {
    let ext = shot::image_kind(data).context("это не PNG и не JPEG")?;
    std::fs::create_dir_all(dir)?;
    let p = util::unique_path(dir, &format!("{} {}", label, util::file_stamp()), ext);
    std::fs::write(&p, data)?;
    entry(&p)
}

/// Сохранить отредактированный скриншот: копией рядом («… правка») или поверх оригинала.
pub fn save_edited(src: &Path, data: &[u8], replace: bool) -> Result<ClipEntry> {
    let ext = shot::image_kind(data).context("это не PNG и не JPEG")?;
    // поверх оригинала — только тем же форматом, иначе расширение файла соврёт
    let src_ext = src.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let same = src_ext == ext || (ext == "jpg" && src_ext == "jpeg");
    let dst = if replace && same {
        src.to_path_buf()
    } else {
        let dir = src.parent().context("нет папки")?;
        let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Скриншот".into());
        util::unique_path(dir, &format!("{stem} правка"), ext)
    };
    // сначала во временный файл: оригинал не испортится, если запись прервётся
    let tmp = dst.with_extension(format!("{ext}.part"));
    std::fs::write(&tmp, data)?;
    if let Err(e) = std::fs::rename(&tmp, &dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("не удалось записать {}", dst.display()));
    }
    entry(&dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_names() {
        assert_eq!(clean_name("  Танк / с двух: выстрелов?  ").unwrap(), "Танк  с двух выстрелов");
        assert_eq!(clean_name("clip...").unwrap(), "clip");
        assert!(clean_name("   ").is_err());
        assert!(clean_name("con").is_err());
        assert!(clean_name(&"a".repeat(200)).is_err());
    }

    #[test]
    fn lists_only_media_and_guards_paths() {
        let dir = std::env::temp_dir().join("clipback-app-test-list");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.join("broken.mp4"), b"x").unwrap();
        std::fs::write(dir.join("clip.part"), b"x").unwrap();
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x0a\0\0\0\x05\xa0".to_vec();
        std::fs::write(dir.join("shot.png"), &png).unwrap();

        let l = list(&dir);
        assert_eq!(l.len(), 2);
        let shot = l.iter().find(|c| c.kind == "shot").unwrap();
        assert_eq!((shot.width, shot.height), (2560, 1440));
        assert!(l.iter().find(|c| c.kind == "video").unwrap().broken);

        let inside = ensure_inside(&dir, &dir.join("shot.png")).unwrap();
        assert!(!inside.to_string_lossy().starts_with(r"\\?\"), "путь должен быть обычным: {}", inside.display());
        assert!(ensure_inside(&dir, &dir.join("notes.txt")).is_err());
        assert!(ensure_inside(&dir, &std::env::temp_dir()).is_err());
        assert!(ensure_inside(&dir, &dir.join("..").join("clipback-app-test-list").join("shot.png")).is_ok());
    }

    #[test]
    fn rename_refuses_to_overwrite() {
        let dir = std::env::temp_dir().join("clipback-app-test-rename");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01".to_vec();
        std::fs::write(dir.join("a.png"), &png).unwrap();
        std::fs::write(dir.join("b.png"), &png).unwrap();
        assert!(rename(&dir.join("a.png"), "b").is_err());
        let e = rename(&dir.join("a.png"), "новое имя").unwrap();
        assert_eq!(e.name, "новое имя");
        assert!(dir.join("новое имя.png").exists());
    }

    #[test]
    fn save_image_checks_format_and_reads_size() {
        let dir = std::env::temp_dir().join("clipback-app-test-png");
        assert!(save_image(&dir, "x", b"not a png").is_err());
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x03".to_vec();
        let e = save_image(&dir, "Игра", &png).unwrap();
        assert_eq!((e.width, e.height), (2, 3));
        assert!(e.name.starts_with("Игра ") && e.path.ends_with(".png"));

        let rgba = vec![200u8; 7 * 5 * 4];
        let jpg = shot::encode_jpeg(&rgba, 7, 5, 90).unwrap();
        let e = save_image(&dir, "Игра", &jpg).unwrap();
        assert!(e.path.ends_with(".jpg"));
        assert_eq!(e.kind, "shot");
        assert_eq!((e.width, e.height), (7, 5));
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    #[test]
    fn edited_shot_saved_as_copy_or_replaced() {
        let dir = std::env::temp_dir().join("clipback-edit-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = |b: u8| [b"\x89PNG\r\n\x1a\n".as_slice(), &[b; 16]].concat();
        let src = dir.join("Игра 2026-10-15 21-00-00.png");
        std::fs::write(&src, png(1)).unwrap();

        let copy = save_edited(&src, &png(2), false).unwrap();
        assert!(copy.path.ends_with("Игра 2026-10-15 21-00-00 правка.png"), "{}", copy.path);
        assert_eq!(std::fs::read(&src).unwrap(), png(1));
        // вторая копия не затирает первую
        let copy2 = save_edited(&src, &png(3), false).unwrap();
        assert_ne!(copy.path, copy2.path);

        save_edited(&src, &png(4), true).unwrap();
        assert_eq!(std::fs::read(&src).unwrap(), png(4));
        assert!(save_edited(&src, b"not a png", true).is_err());
        assert_eq!(std::fs::read(&src).unwrap(), png(4));
    }
}
