//! Избранное и автоочистка папки клипов.
//!
//! Избранное — список имён файлов в `favorites.json` рядом с настройками (сами клипы не меняются).
//! Автоочистка выключена по умолчанию; когда включена — отправляет в корзину самые старые клипы,
//! пока папка не уложится в лимит по размеру и возрасту. Избранное и свежие клипы она не трогает.

use crate::clips::ClipEntry;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub struct Favorites {
    names: BTreeSet<String>,
    path: PathBuf,
}

fn file_name(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase())
}

impl Favorites {
    pub fn load(path: PathBuf) -> Self {
        let names = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { names, path }
    }

    fn save(&self) {
        let _ = std::fs::write(&self.path, serde_json::to_vec_pretty(&self.names).unwrap_or_default());
    }

    pub fn contains(&self, p: &Path) -> bool {
        file_name(p).is_some_and(|n| self.names.contains(&n))
    }

    pub fn set(&mut self, p: &Path, on: bool) {
        let Some(n) = file_name(p) else { return };
        let changed = if on { self.names.insert(n) } else { self.names.remove(&n) };
        if changed {
            self.save();
        }
    }

    /// Файл переименовали — звёздочка переезжает за ним.
    pub fn renamed(&mut self, from: &Path, to: &Path) {
        if self.contains(from) {
            self.set(from, false);
            self.set(to, true);
        }
    }

    pub fn mark(&self, list: &mut [ClipEntry]) {
        for e in list {
            e.favorite = self.contains(Path::new(&e.path));
        }
    }
}

/// Не трогаем клипы младше часа: только что сохранённое не должно исчезнуть из-за лимита.
const FRESH_MS: u64 = 3_600_000;

/// Какие клипы убрать: старше `max_days` или самые старые, пока папка больше `max_bytes`.
pub fn clean_plan(list: &[ClipEntry], max_bytes: Option<u64>, max_days: Option<u32>, now_ms: u64) -> Vec<PathBuf> {
    let mut cands: Vec<&ClipEntry> = list.iter().filter(|e| !e.favorite && now_ms.saturating_sub(e.modified) > FRESH_MS).collect();
    cands.sort_by_key(|e| e.modified); // самые старые первыми
    let mut total: u64 = list.iter().map(|e| e.size).sum();
    let mut out = Vec::new();
    for e in cands {
        let too_old = max_days.is_some_and(|d| now_ms.saturating_sub(e.modified) > d as u64 * 86_400_000);
        let too_big = max_bytes.is_some_and(|m| total > m);
        if too_old || too_big {
            total -= e.size;
            out.push(PathBuf::from(&e.path));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    fn clip(name: &str, age_days: f64, size: u64, favorite: bool) -> ClipEntry {
        let now = 100 * DAY;
        ClipEntry {
            path: format!(r"C:\clips\{name}.mp4"),
            name: name.into(),
            kind: "video",
            size,
            modified: now - (age_days * DAY as f64) as u64,
            favorite,
            ..ClipEntry::default()
        }
    }

    #[test]
    fn removes_oldest_until_under_size_limit() {
        let list = vec![clip("a", 10.0, 40, false), clip("b", 5.0, 40, false), clip("c", 1.0, 40, false)];
        let plan = clean_plan(&list, Some(90), None, 100 * DAY);
        assert_eq!(plan, vec![PathBuf::from(r"C:\clips\a.mp4")]);
    }

    #[test]
    fn keeps_favorites_and_fresh_clips() {
        let list = vec![clip("fav", 50.0, 100, true), clip("old", 40.0, 10, false), clip("new", 0.01, 100, false)];
        // лимит недостижим без избранного и свежего — их всё равно не трогаем
        let plan = clean_plan(&list, Some(1), Some(30), 100 * DAY);
        assert_eq!(plan, vec![PathBuf::from(r"C:\clips\old.mp4")]);
    }

    #[test]
    fn age_limit_alone() {
        let list = vec![clip("a", 31.0, 1, false), clip("b", 29.0, 1, false)];
        assert_eq!(clean_plan(&list, None, Some(30), 100 * DAY), vec![PathBuf::from(r"C:\clips\a.mp4")]);
        assert!(clean_plan(&list, None, None, 100 * DAY).is_empty());
    }

    #[test]
    fn favorites_survive_rename() {
        let dir = std::env::temp_dir().join("clipback-fav-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("favorites.json");
        let _ = std::fs::remove_file(&path);
        let mut f = Favorites::load(path.clone());
        f.set(Path::new(r"C:\clips\Игра 1.mp4"), true);
        f.renamed(Path::new(r"C:\clips\Игра 1.mp4"), Path::new(r"C:\clips\Лучший момент.mp4"));
        let f = Favorites::load(path);
        assert!(f.contains(Path::new(r"D:\другая\лучший момент.MP4")));
        assert!(!f.contains(Path::new(r"C:\clips\Игра 1.mp4")));
    }
}
