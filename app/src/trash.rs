//! Удаление с возможностью «Вернуть».
//!
//! Клип сначала мгновенно переезжает в скрытую папку `.clipback-trash` рядом с клипами
//! (переименование на том же диске — без копирования). Если за несколько секунд не нажали
//! «Вернуть», он уходит в обычную корзину Windows. При выходе из программы и при следующем
//! запуске всё, что осталось в скрытой папке, тоже отправляется в корзину.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const DIR: &str = ".clipback-trash";
/// Сколько секунд можно передумать.
pub const UNDO_SECS: u64 = 10;

struct Item {
    batch: u64,
    original: PathBuf,
    staged: PathBuf,
}

#[derive(Default)]
pub struct Trash {
    items: Mutex<Vec<Item>>,
    next: AtomicU64,
}

fn staged_dir(p: &Path) -> Result<PathBuf> {
    let d = p.parent().context("нет папки")?.join(DIR);
    if !d.exists() {
        std::fs::create_dir_all(&d)?;
        hide(&d);
    }
    Ok(d)
}

fn hide(p: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let _ = SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_ATTRIBUTE_HIDDEN);
    }
}

/// В корзину Windows — под исходным именем, чтобы «Восстановить» в корзине вернуло файл на место.
fn to_recycle(original: &Path, staged: &Path) -> Result<()> {
    let target = if !original.exists() && std::fs::rename(staged, original).is_ok() { original } else { staged };
    crate::winutil::move_to_recycle_bin(target)
}

impl Trash {
    /// Убрать клипы. Возвращает номер пачки (для «Вернуть») и сколько файлов убрано.
    pub fn stage(&self, paths: &[PathBuf]) -> (u64, Vec<PathBuf>, Vec<String>) {
        let batch = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (mut done, mut errors) = (Vec::new(), Vec::new());
        for p in paths {
            let r = (|| -> Result<PathBuf> {
                let name = p.file_name().context("нет имени")?.to_string_lossy().into_owned();
                let staged = staged_dir(p)?.join(format!("{batch}-{name}"));
                std::fs::rename(p, &staged).context("файл занят другой программой")?;
                Ok(staged)
            })();
            match r {
                Ok(staged) => {
                    self.items.lock().unwrap_or_else(|e| e.into_inner()).push(Item { batch, original: p.clone(), staged });
                    done.push(p.clone());
                }
                Err(e) => errors.push(format!("{}: {e:#}", p.display())),
            }
        }
        (batch, done, errors)
    }

    fn take(&self, batch: u64) -> Vec<Item> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest): (Vec<Item>, Vec<Item>) = items.drain(..).partition(|i| i.batch == batch);
        *items = rest;
        mine
    }

    /// «Вернуть»: файлы встают на прежнее место. Возвращает вернувшиеся пути.
    pub fn restore(&self, batch: u64) -> Vec<PathBuf> {
        let mut back = Vec::new();
        for i in self.take(batch) {
            let target = if i.original.exists() {
                // на месте уже другой файл с тем же именем — возвращаем рядом под новым
                let stem = i.original.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let ext = i.original.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                clipback_core::util::unique_path(i.original.parent().unwrap_or(Path::new(".")), &stem, &ext)
            } else {
                i.original.clone()
            };
            match std::fs::rename(&i.staged, &target) {
                Ok(()) => back.push(target),
                Err(e) => crate::log(format!("не удалось вернуть {}: {e}", i.original.display())),
            }
        }
        back
    }

    /// Время вышло — пачка уходит в корзину Windows. Возвращает отправленные исходные пути.
    pub fn finalize(&self, batch: u64) -> Vec<PathBuf> {
        let mut gone = Vec::new();
        for i in self.take(batch) {
            match to_recycle(&i.original, &i.staged) {
                Ok(()) => gone.push(i.original),
                Err(e) => crate::log(format!("в корзину не ушло {}: {e:#}", i.original.display())),
            }
        }
        gone
    }

    /// Всё, что ещё ждёт, — в корзину (при выходе).
    pub fn finalize_all(&self) {
        let batches: Vec<u64> = self.items.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|i| i.batch).collect();
        for b in batches {
            self.finalize(b);
        }
    }
}

/// Остатки от прошлого запуска (программу закрыли до истечения времени) — в корзину.
/// Смотрим и папку клипов, и папки игр внутри неё.
pub fn flush_leftovers(out_dir: &Path) {
    flush_dir(out_dir);
    if let Ok(rd) = std::fs::read_dir(out_dir) {
        for e in rd.flatten() {
            if e.path().is_dir() && e.file_name() != DIR {
                flush_dir(&e.path());
            }
        }
    }
}

fn flush_dir(out_dir: &Path) {
    let d = out_dir.join(DIR);
    let Ok(rd) = std::fs::read_dir(&d) else { return };
    for e in rd.flatten() {
        let staged = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        // «7-Игра 2026-….mp4» → «Игра 2026-….mp4»
        let orig_name = name.split_once('-').map(|(_, n)| n.to_string()).unwrap_or(name);
        let _ = to_recycle(&out_dir.join(orig_name), &staged);
    }
    let _ = std::fs::remove_dir(&d);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn staged_files_come_back_on_undo() {
        let d = dir("clipback-trash-undo");
        let a = d.join("Игра 1.mp4");
        let b = d.join("Игра 2.jpg");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let t = Trash::default();
        let (batch, done, errors) = t.stage(&[a.clone(), b.clone()]);
        assert_eq!(done.len(), 2);
        assert!(errors.is_empty());
        assert!(!a.exists() && !b.exists());
        // скрытая папка не мешает списку клипов: в ней наши файлы
        assert_eq!(std::fs::read_dir(d.join(DIR)).unwrap().count(), 2);
        let back = t.restore(batch);
        assert_eq!(back.len(), 2);
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
        // второй раз вернуть нечего
        assert!(t.restore(batch).is_empty());
    }

    #[test]
    fn undo_does_not_overwrite_a_new_file_with_same_name() {
        let d = dir("clipback-trash-clash");
        let a = d.join("Клип.mp4");
        std::fs::write(&a, b"old").unwrap();
        let t = Trash::default();
        let (batch, ..) = t.stage(&[a.clone()]);
        std::fs::write(&a, b"new").unwrap();
        let back = t.restore(batch);
        assert_eq!(std::fs::read(&a).unwrap(), b"new");
        assert_ne!(back[0], a);
        assert_eq!(std::fs::read(&back[0]).unwrap(), b"old");
    }

    #[test]
    fn missing_file_reports_error_and_others_still_go() {
        let d = dir("clipback-trash-missing");
        let a = d.join("есть.mp4");
        std::fs::write(&a, b"a").unwrap();
        let t = Trash::default();
        let (_, done, errors) = t.stage(&[d.join("нет.mp4"), a.clone()]);
        assert_eq!(done, vec![a]);
        assert_eq!(errors.len(), 1);
    }
}
