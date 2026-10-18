//! Место на диске и папка с клипами: проверить до сохранения, а не узнать по битому файлу.
//!
//! - Перед сохранением повтора, скриншота и отрезка — хватит ли места (ожидаемый размер + запас).
//! - Во время полноценной записи — раз в 10 с; места мало — запись останавливается
//!   и файл дописывается целым, пока на диске ещё есть куда записать оглавление MP4.
//! - Папка с клипами недоступна (диск отключён, нет прав) — плашка в окне и одно уведомление;
//!   буфер в памяти продолжает работать, а когда диск вернётся, всё заработает само.

use crate::{emit_state, lock, log, show_toast, winutil};
use clipback_core::Command;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::AppHandle;

/// Запас свободного места сверх ожидаемого размера файла: Windows и другим программам
/// тоже нужно куда-то писать, а оценка размера не точная.
pub const MARGIN: u64 = 256 * 1024 * 1024;
/// Сколько секунд записи должно помещаться на диск, чтобы полноценная запись продолжалась.
pub const RECORD_HEADROOM_SECS: u64 = 60;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FolderState {
    #[default]
    Ok,
    /// Нет самого диска (отключён, флешка вынута, сетевой диск недоступен).
    NoDrive,
    /// Диск есть, но в папку нельзя записать (нет прав, путь занят файлом).
    NoAccess,
}

/// Состояние папки по результатам проверок.
pub fn classify(drive_exists: bool, dir_ok: bool, writable: bool) -> FolderState {
    if !drive_exists {
        FolderState::NoDrive
    } else if !dir_ok || !writable {
        FolderState::NoAccess
    } else {
        FolderState::Ok
    }
}

/// Корень тома: `G:\` или `\\server\share\`.
fn volume_root(dir: &Path) -> Option<PathBuf> {
    match dir.components().next()? {
        Component::Prefix(p) => Some(PathBuf::from(p.as_os_str()).join("\\")),
        _ => None,
    }
}

/// Как назвать диск в сообщении: «G:» или «\\server\share».
pub fn drive_name(dir: &Path) -> String {
    match dir.components().next() {
        Some(Component::Prefix(p)) => p.as_os_str().to_string_lossy().trim_start_matches(r"\\?\").to_string(),
        _ => dir.display().to_string(),
    }
}

fn probe_write(dir: &Path) -> bool {
    let p = dir.join(".clipback-write-test.tmp");
    let ok = std::fs::write(&p, b"ok").is_ok();
    let _ = std::fs::remove_file(&p);
    ok
}

/// Проверить папку: есть ли диск, есть ли папка (нет — создаём), можно ли в неё писать.
pub fn check_folder(dir: &Path) -> FolderState {
    let drive = volume_root(dir).is_none_or(|r| r.exists());
    if !drive {
        return classify(false, false, false);
    }
    let dir_ok = dir.is_dir() || (std::fs::create_dir_all(dir).is_ok() && dir.is_dir());
    classify(true, dir_ok, dir_ok && probe_write(dir))
}

/// Хватит ли места на файл ожидаемого размера. Свободное место неизвестно — не мешаем.
pub fn enough_space(free: Option<u64>, expected: u64) -> bool {
    free.is_none_or(|f| f >= expected.saturating_add(MARGIN))
}

/// Байт за `secs` секунд видео с битрейтом `mbps` (звук — ещё ~1 %, он внутри запаса).
pub fn bytes_for(mbps: u32, secs: u64) -> u64 {
    mbps as u64 * 1_000_000 / 8 * secs
}

/// Полноценную запись пора останавливать: на диске меньше минуты записи (плюс запас).
pub fn record_must_stop(free: Option<u64>, mbps: u32) -> bool {
    !enough_space(free, bytes_for(mbps, RECORD_HEADROOM_SECS))
}

/// Ожидаемый размер отрезка: доля исходного файла по длине (перекодирование в меньшее
/// качество даст меньше), не больше лимита «для Discord».
pub fn trim_expected(src_bytes: u64, src_secs: f64, start: f64, end: f64, limit: Option<u64>) -> u64 {
    let part = if src_secs > 0.0 { ((end - start) / src_secs).clamp(0.0, 1.0) } else { 1.0 };
    let est = (src_bytes as f64 * part).ceil() as u64;
    limit.map_or(est, |l| est.min(l))
}

/// Сколько оперативной памяти в компьютере — интерфейс сравнивает с размером буфера.
pub fn ram_total() -> Option<u64> {
    static T: OnceLock<Option<u64>> = OnceLock::new();
    *T.get_or_init(clipback_core::sysload::total_ram)
}

/// Причина, почему в папку нельзя сохранять (короткая фраза для уведомления).
pub fn folder_reason(state: FolderState, dir: &Path) -> Option<String> {
    match state {
        FolderState::Ok => None,
        FolderState::NoDrive => Some(format!("Диск {} не найден", drive_name(dir))),
        FolderState::NoAccess => Some("Нет доступа к папке для клипов".into()),
    }
}

/// Текст плашки в главном окне.
pub fn banner_text(state: FolderState, dir: &Path) -> Option<String> {
    let d = dir.display();
    match state {
        FolderState::Ok => None,
        FolderState::NoDrive => Some(format!(
            "Диск {} не найден — клипы некуда сохранять ({d}). Подключите диск или выберите другую папку. Буфер записи продолжает работать.",
            drive_name(dir)
        )),
        FolderState::NoAccess => Some(format!("Нет доступа к папке для клипов ({d}) — клипы не сохраняются. Выберите другую папку.")),
    }
}

// ---------- наблюдение за папкой ----------

struct Watch {
    dir: PathBuf,
    state: FolderState,
    /// Когда последний раз показывали уведомление — не чаще раза в 30 минут.
    toasted: Option<Instant>,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);
/// Полноценную запись остановили из-за места — сказать об этом, когда файл допишется.
static LOW_STOP: AtomicBool = AtomicBool::new(false);
static LAST_RECORD_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

fn watch_lock() -> std::sync::MutexGuard<'static, Option<Watch>> {
    WATCH.lock().unwrap_or_else(|e| e.into_inner())
}

/// Текст плашки, если с папкой беда (для состояния, которое видит интерфейс).
pub fn problem_text() -> Option<String> {
    watch_lock().as_ref().and_then(|w| banner_text(w.state, &w.dir))
}

fn out_dir(app: &AppHandle) -> PathBuf {
    lock(app).settings.out_dir.clone()
}

/// Полная проверка папки. При смене состояния — журнал, плашка, уведомление.
pub fn recheck(app: &AppHandle) -> FolderState {
    let dir = out_dir(app);
    let state = check_folder(&dir);
    let (changed, toast) = {
        let mut g = watch_lock();
        let w = g.get_or_insert_with(|| Watch { dir: dir.clone(), state: FolderState::Ok, toasted: None });
        let changed = w.state != state || w.dir != dir;
        let toast = changed
            && state != FolderState::Ok
            && w.toasted.is_none_or(|t| t.elapsed() >= Duration::from_secs(30 * 60));
        if toast {
            w.toasted = Some(Instant::now());
        }
        w.dir = dir.clone();
        w.state = state;
        (changed, toast)
    };
    if changed {
        match state {
            FolderState::Ok => {
                log(format!("папка для клипов доступна: {}", dir.display()));
                crate::clips_changed(app);
            }
            s => log(format!("папка для клипов недоступна ({s:?}): {}", dir.display())),
        }
        emit_state(app);
    }
    if toast {
        if let Some(r) = folder_reason(state, &dir) {
            show_toast(app, &r, "Клипы не сохранятся — выберите папку в Clipback", "error");
        }
    }
    state
}

/// Раз в 5 с: папка в порядке — быстрая проверка, что она на месте; нет — полная
/// (диск подключили — снова сохраняем, без перезапуска).
pub fn watch(app: AppHandle) {
    std::thread::Builder::new()
        .name("clipback-storage".into())
        .spawn(move || {
            recheck(&app);
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let dir = out_dir(&app);
                let ok = watch_lock().as_ref().is_some_and(|w| w.state == FolderState::Ok && w.dir == dir);
                if !ok || !dir.is_dir() {
                    recheck(&app);
                }
            }
        })
        .ok();
}

/// Можно ли сохранить файл ожидаемого размера. `None` — можно, иначе причина.
pub fn save_blocked(app: &AppHandle, expected: u64) -> Option<String> {
    let dir = out_dir(app);
    if let Some(r) = folder_reason(recheck(app), &dir) {
        return Some(r);
    }
    let free = winutil::free_space(&dir);
    if enough_space(free, expected) {
        return None;
    }
    log(format!(
        "мало места: свободно {} МБ, нужно {} МБ + запас — не сохраняю",
        free.unwrap_or(0) / 1_000_000,
        expected / 1_000_000
    ));
    Some(format!("Мало места на диске {}", drive_name(&dir)))
}

/// То же для команд интерфейса: ошибка одной строкой.
pub fn ensure_space(app: &AppHandle, expected: u64, what: &str) -> Result<(), String> {
    save_blocked(app, expected).map_or(Ok(()), |r| Err(format!("{r} — {what}")))
}

/// Ожидаемый размер повтора: столько, сколько сейчас в буфере.
pub fn replay_expected(app: &AppHandle) -> u64 {
    let i = lock(app);
    match &i.status {
        Some(s) => s.buffer_bytes as u64,
        None => {
            let h = i.capture.as_ref().map_or(1080, |c| c.height);
            bytes_for(i.settings.bitrate_mbps(h), (i.settings.buffer_minutes * 60.0).round() as u64)
        }
    }
}

/// Полноценную запись можно начать, только если на диск влезает хотя бы минута.
pub fn record_blocked(app: &AppHandle) -> Option<String> {
    let mbps = {
        let i = lock(app);
        i.settings.bitrate_mbps(i.capture.as_ref().map_or(1080, |c| c.height))
    };
    save_blocked(app, bytes_for(mbps, RECORD_HEADROOM_SECS))
}

/// Скриншот: не больше несжатой картинки.
pub fn shot_expected(app: &AppHandle) -> u64 {
    lock(app).capture.as_ref().map_or(3840 * 2160 * 4, |c| c.width as u64 * c.height as u64 * 4)
}

/// Отрезок файла `src` с `start` по `end` с качеством из окна экспорта.
pub fn trim_expected_for(src: &Path, start: f64, end: f64, quality: Option<&str>) -> u64 {
    let bytes = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let secs = clipback_core::mux::clip_info(src).map(|i| i.duration).unwrap_or(0.0);
    let limit = quality.and_then(|q| q.strip_prefix("discord:")).and_then(|v| v.parse::<u64>().ok()).map(|mb| mb * 1_000_000);
    trim_expected(bytes, secs, start, end, limit)
}

/// Вызывается на каждое `Event::Status` (раз в 2 с). Идёт полноценная запись и места
/// почти не осталось — останавливаем её, пока файл ещё можно дописать целым.
pub fn on_status(app: &AppHandle) {
    {
        let mut last = LAST_RECORD_CHECK.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
            return;
        }
        *last = Some(Instant::now());
    }
    let (dir, mbps, tx) = {
        let i = lock(app);
        if i.record_started.is_none() || LOW_STOP.load(Ordering::SeqCst) {
            return;
        }
        let Some(r) = &i.recorder else { return };
        let h = i.capture.as_ref().map_or(1080, |c| c.height);
        (i.settings.out_dir.clone(), i.settings.bitrate_mbps(h), r.sender())
    };
    let free = winutil::free_space(&dir);
    if record_must_stop(free, mbps) {
        log(format!("мало места на диске ({} МБ) — останавливаю запись", free.unwrap_or(0) / 1_000_000));
        LOW_STOP.store(true, Ordering::SeqCst);
        let _ = tx.send(Command::StopRecord);
        show_toast(app, &format!("Мало места на диске {}", drive_name(&dir)), "Запись остановлена — файл сохраняется", "error");
    }
}

/// Запись закончилась: её остановили из-за места? (Сбрасывает признак.)
pub fn take_low_stop() -> bool {
    LOW_STOP.swap(false, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;

    #[test]
    fn folder_states() {
        assert_eq!(classify(false, false, false), FolderState::NoDrive);
        assert_eq!(classify(false, true, true), FolderState::NoDrive);
        assert_eq!(classify(true, false, false), FolderState::NoAccess);
        assert_eq!(classify(true, true, false), FolderState::NoAccess);
        assert_eq!(classify(true, true, true), FolderState::Ok);
    }

    #[test]
    fn real_folders() {
        let base = std::env::temp_dir().join(format!("clipback-storage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // папки ещё нет — создаётся
        let dir = base.join("клипы").join("Minecraft");
        assert_eq!(check_folder(&dir), FolderState::Ok);
        assert!(dir.is_dir());
        assert!(!dir.join(".clipback-write-test.tmp").exists());
        // на месте папки файл — папку не создать
        let file = base.join("файл");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(check_folder(&file.join("клипы")), FolderState::NoAccess);
        let _ = std::fs::remove_dir_all(&base);
        // диска нет
        if let Some(l) = ('D'..='Z').rev().find(|l| !Path::new(&format!("{l}:\\")).exists()) {
            let dir = PathBuf::from(format!("{l}:\\MEDIA+\\клипы ClipBack"));
            assert_eq!(check_folder(&dir), FolderState::NoDrive);
            assert_eq!(folder_reason(FolderState::NoDrive, &dir).unwrap(), format!("Диск {l}: не найден"));
        }
    }

    #[test]
    fn drive_names() {
        assert_eq!(drive_name(Path::new(r"G:\MEDIA+\клипы ClipBack")), "G:");
        assert_eq!(drive_name(Path::new(r"\\nas\video\clips")), r"\\nas\video");
        assert_eq!(volume_root(Path::new(r"G:\MEDIA+")).unwrap(), PathBuf::from(r"G:\"));
        assert!(banner_text(FolderState::Ok, Path::new(r"G:\x")).is_none());
        assert!(banner_text(FolderState::NoDrive, Path::new(r"G:\x")).unwrap().starts_with("Диск G: не найден"));
    }

    #[test]
    fn space_thresholds() {
        // неизвестно — не мешаем
        assert!(enough_space(None, 10 * GB));
        // повтор 5 минут при 8 Мбит/с ≈ 300 МБ
        let clip = bytes_for(8, 300);
        assert_eq!(clip, 300 * MB);
        assert!(enough_space(Some(50 * GB), clip));
        assert!(enough_space(Some(clip + MARGIN), clip));
        assert!(!enough_space(Some(clip + MARGIN - 1), clip));
        assert!(!enough_space(Some(100 * MB), clip));
        // скриншот на почти полном диске (но с запасом) проходит
        assert!(enough_space(Some(320 * MB), 33 * MB));
        assert!(!enough_space(Some(0), 0));
    }

    #[test]
    fn record_stops_before_disk_is_full() {
        // 14 Мбит/с: минута ≈ 105 МБ, плюс запас
        assert!(!record_must_stop(Some(5 * GB), 14));
        assert!(!record_must_stop(None, 14));
        assert!(record_must_stop(Some(300 * MB), 14));
        assert!(!record_must_stop(Some(400 * MB), 14));
        assert!(record_must_stop(Some(400 * MB), 120));
    }

    #[test]
    fn trim_size_estimate() {
        // 10 с из 10-минутного файла в 1 ГБ
        assert_eq!(trim_expected(GB, 600.0, 100.0, 110.0, None), 16_666_667);
        assert_eq!(trim_expected(GB, 600.0, 0.0, 600.0, None), GB);
        assert_eq!(trim_expected(GB, 600.0, 0.0, 900.0, None), GB);
        assert_eq!(trim_expected(GB, 0.0, 0.0, 10.0, None), GB);
        assert_eq!(trim_expected(GB, 600.0, 0.0, 600.0, Some(25 * MB)), 25 * MB);
    }
}
