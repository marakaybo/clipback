//! Приложение Clipback: главное окно, оверлей по Alt+Z, уведомления, трей и горячие клавиши.
//! Запись идёт в ядре (`clipback-core`), здесь — управление им и интерфейс.

mod clips;
mod indicator;
mod library;
mod nvidia;
mod power;
mod report;
mod settings;
mod sound;
mod storage;
mod trash;
mod update;
mod widgets;
mod winutil;

use base64::Engine;
use clipback_core::{gpu, util, Command, Config, Event, Recorder};
use serde::Serialize;
use settings::Settings;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

// ---------- состояние ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
    Overlay,
    Save,
    Screenshot,
    Record,
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureInfo {
    encoder: String,
    gpu: String,
    monitor: String,
    width: u32,
    height: u32,
    fps: u32,
    audio: Vec<String>,
    audio_error: Option<String>,
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveStatus {
    buffered_secs: f64,
    buffer_bytes: usize,
    fps: f64,
    dropped: u64,
}

/// Игра на весь экран, которая не попадает в захват (см. `Event::CaptureBlocked`).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockedGame {
    game: String,
    /// Программа игры («javaw.exe»), если удалось узнать.
    exe: Option<String>,
    /// Видеокарта NVIDIA — можно исправить настройкой драйвера (`nvidia.rs`).
    nvidia: bool,
    /// Настройка драйвера для этой программы уже включена — осталось перезапустить игру.
    driver_ok: bool,
}

#[derive(Default)]
struct Inner {
    settings: Settings,
    recorder: Option<Recorder>,
    starting: bool,
    capture: Option<CaptureInfo>,
    status: Option<LiveStatus>,
    error: Option<String>,
    /// Игра на весь экран, которая сейчас не попадает в захват (плашка с инструкцией).
    capture_blocked: Option<BlockedGame>,
    /// О каких играх уже показали уведомление — не чаще раза за запуск программы.
    blocked_toasted: std::collections::HashSet<String>,
    last_game: Option<String>,
    hotkeys: Vec<(Shortcut, Action)>,
    hotkey_errors: Vec<String>,
    /// Полноценная запись идёт с этого момента (мс с 1970 года).
    record_started: Option<u64>,
    /// Запись попросили, когда захват был выключен: начать её сразу после запуска.
    record_after_start: bool,
    /// Старый рекордер ещё дописывает файлы в фоне (`stop_recording`).
    stopping: bool,
    /// Включить захват, как только закончится остановка или текущий запуск
    /// (перезапуск после смены настроек).
    start_pending: bool,
}

struct AppState {
    inner: Mutex<Inner>,
    settings_path: PathBuf,
    /// Номер текущего запуска записи — события от старого рекордера игнорируются.
    generation: AtomicU64,
    toast_seq: AtomicU64,
    overlay_seq: AtomicU64,
    /// Форма волны клипов: путь+время изменения -> пики дорожек.
    peaks: Mutex<std::collections::HashMap<String, Vec<Vec<f32>>>>,
    rec_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
    record_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    favorites: Mutex<library::Favorites>,
    /// Скрытые записи (тот же список имён, что и избранное, в `hidden.json`).
    hidden: Mutex<library::Favorites>,
    trash: trash::Trash,
    /// Поток, который дописывает и закрывает остановленный рекордер.
    stopper: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StateDto {
    recording: bool,
    starting: bool,
    capture: Option<CaptureInfo>,
    status: Option<LiveStatus>,
    error: Option<String>,
    capture_blocked: Option<BlockedGame>,
    last_game: Option<String>,
    hotkey_errors: Vec<String>,
    record_started: Option<u64>,
    /// Свободно на диске с клипами (байты).
    disk_free: Option<u64>,
    /// Папка с клипами недоступна — текст плашки (`storage.rs`).
    folder_problem: Option<String>,
    /// Оперативной памяти в компьютере (байты) — для оценки буфера в настройках.
    mem_total: Option<u64>,
    settings: Settings,
    version: &'static str,
    portable: bool,
}

fn dto(i: &Inner) -> StateDto {
    StateDto {
        recording: i.recorder.is_some(),
        starting: i.starting || i.start_pending,
        capture: i.capture.clone(),
        status: i.status.clone(),
        error: i.error.clone(),
        capture_blocked: i.capture_blocked.clone(),
        last_game: i.last_game.clone(),
        hotkey_errors: i.hotkey_errors.clone(),
        record_started: i.record_started,
        disk_free: winutil::free_space(&i.settings.out_dir),
        folder_problem: storage::problem_text(),
        mem_total: storage::ram_total(),
        settings: i.settings.clone(),
        version: env!("CARGO_PKG_VERSION"),
        portable: update::is_portable(),
    }
}

fn st(app: &AppHandle) -> &AppState {
    app.state::<AppState>().inner()
}

fn lock(app: &AppHandle) -> std::sync::MutexGuard<'_, Inner> {
    // отравленный мьютекс (паника в другом потоке) не должен ронять всё приложение
    st(app).inner.lock().unwrap_or_else(|e| e.into_inner())
}

fn emit_state(app: &AppHandle) {
    let d = dto(&lock(app));
    update_tray(app, &d);
    let _ = app.emit("state", d);
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusDto {
    status: Option<LiveStatus>,
    disk_free: Option<u64>,
}

/// Тик буфера (раз в 2 с, пока идёт запись): маленькое событие и только видимым окнам.
/// Полное состояние с настройками уходит лишь при настоящих изменениях (`emit_state`),
/// иначе три WebView разбирали его каждые 2 с, а открытые настройки сбрасывались.
fn emit_status(app: &AppHandle) {
    let d = dto(&lock(app));
    update_tray(app, &d);
    let s = StatusDto { status: d.status, disk_free: d.disk_free };
    for label in ["main", "overlay"] {
        if app.get_webview_window(label).is_some_and(|w| w.is_visible().unwrap_or(false)) {
            let _ = app.emit_to(label, "status", &s);
        }
    }
}

fn clips_changed(app: &AppHandle) {
    let _ = app.emit("clips-changed", ());
}

// ---------- журнал ----------

static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Размер журнала: при 2 МБ он откладывается в clipback.old.log прямо во время работы,
/// а не только при запуске (компьютер неделями не выключают).
static LOG_BYTES: AtomicU64 = AtomicU64::new(0);
const LOG_MAX: u64 = 2_000_000;

pub fn log(msg: impl AsRef<str>) {
    let line = format!("[{}] {}
", util::file_stamp(), msg.as_ref());
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
    let Ok(guard) = LOG_PATH.lock() else { return };
    let Some(p) = guard.as_ref() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = f.write_all(line.as_bytes());
    }
    if LOG_BYTES.fetch_add(line.len() as u64, Ordering::Relaxed) + line.len() as u64 > LOG_MAX {
        let _ = std::fs::rename(p, p.with_file_name("clipback.old.log"));
        LOG_BYTES.store(0, Ordering::Relaxed);
    }
}

fn init_log(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let p = dir.join("clipback.log");
    // журнал не разрастается: больше 2 МБ — откладываем его в clipback.old.log и начинаем новый
    if std::fs::metadata(&p).map(|m| m.len() > LOG_MAX).unwrap_or(false) {
        let _ = std::fs::rename(&p, dir.join("clipback.old.log"));
    }
    LOG_BYTES.store(std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0), Ordering::Relaxed);
    *LOG_PATH.lock().unwrap() = Some(p);
    std::panic::set_hook(Box::new(|info| log(format!("ПАНИКА: {info}"))));
}

// ---------- запись ----------

fn capture_config(s: &Settings) -> Config {
    let height = gpu::list_monitors()
        .ok()
        .and_then(|m| m.into_iter().find(|(i, ..)| *i == s.monitor).map(|(_, _, _, h)| h))
        .unwrap_or(1080);
    Config {
        monitor: s.monitor,
        fps: s.fps,
        bitrate_mbps: s.bitrate_mbps(height),
        buffer_secs: (s.buffer_minutes * 60.0).round() as u32,
        out_dir: s.out_dir.clone(),
        max_height: s.max_height,
        audio: s.audio_config(),
        cursor: s.record_cursor,
    }
}

fn start_recording(app: &AppHandle) {
    let cfg = {
        let mut i = lock(app);
        if i.recorder.is_some() {
            return;
        }
        if i.starting || i.stopping {
            // запуск или остановка ещё идут — запустим с текущими настройками, когда закончатся
            i.start_pending = true;
            drop(i);
            emit_state(app);
            return;
        }
        i.starting = true;
        i.error = None;
        capture_config(&i.settings)
    };
    emit_state(app);
    let generation = st(app).generation.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        log(format!("запуск записи: {} к/с, {} Мбит/с, буфер {} с", cfg.fps, cfg.bitrate_mbps, cfg.buffer_secs));
        match Recorder::start(cfg) {
            Ok((mut rec, started)) => {
                let (_, dummy) = mpsc::channel();
                let events = std::mem::replace(&mut rec.events, dummy);
                if start_was_cancelled(&app, generation) {
                    // пока шёл запуск, запись выключили или поменяли настройки
                    drop(rec);
                    log("запуск записи отменён");
                    finish_cancelled_start(&app);
                    return;
                }
                {
                    let mut i = lock(&app);
                    i.starting = false;
                    if let Event::Started { encoder, gpu, monitor, width, height, fps, audio, audio_error } = started {
                        log(format!("запись идёт: {monitor} {width}×{height} {fps} к/с, {encoder} на {gpu}; звук: {}", if audio.is_empty() { "нет".to_string() } else { audio.join(", ") }));
                        if let Some(e) = &audio_error {
                            log(format!("звук не работает: {e}"));
                        }
                        i.capture = Some(CaptureInfo { encoder, gpu, monitor, width, height, fps, audio, audio_error });
                    }
                    if std::mem::take(&mut i.record_after_start) {
                        let label = i.last_game.clone();
                        let dir = Some(util::clip_dir(&i.settings.out_dir, label.as_deref(), i.settings.game_folders));
                        let _ = rec.sender().send(Command::StartRecord { label, dir });
                    }
                    i.recorder = Some(rec);
                }
                emit_state(&app);
                forward_events(app, events, generation);
            }
            Err(e) => {
                log(format!("запись не запустилась: {e:#}"));
                if start_was_cancelled(&app, generation) {
                    finish_cancelled_start(&app);
                    return;
                }
                {
                    let mut i = lock(&app);
                    i.starting = false;
                    i.record_after_start = false;
                    i.error = Some(format!("{e:#}"));
                }
                emit_state(&app);
                retry_recording_later(&app, generation);
            }
        }
    });
}

/// Запись упала или не запустилась (монитор ещё не готов после включения компьютера,
/// видеокарта сбросила устройство) — пробуем снова через 5 с, если пользователь за это
/// время ничего не включал и не выключал. Не больше 20 попыток в час.
fn retry_recording_later(app: &AppHandle, generation: u64) {
    static TRIES: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
    {
        let mut t = TRIES.lock().unwrap_or_else(|e| e.into_inner());
        t.retain(|x| x.elapsed() < Duration::from_secs(3600));
        if t.len() >= 20 {
            log("запись: 20 перезапусков за час — больше не пробую, нужна помощь пользователя");
            return;
        }
        t.push(Instant::now());
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        if st(&app).generation.load(Ordering::SeqCst) == generation && lock(&app).recorder.is_none() {
            log("запись: пробую запустить снова");
            start_recording(&app);
        }
    });
}

/// Генерация сменилась, пока шёл `Recorder::start`: снимаем флаг запуска.
fn start_was_cancelled(app: &AppHandle, generation: u64) -> bool {
    let mut i = lock(app);
    if st(app).generation.load(Ordering::SeqCst) == generation {
        i.start_pending = false;
        return false;
    }
    i.starting = false;
    true
}

fn finish_cancelled_start(app: &AppHandle) {
    let pending = std::mem::take(&mut lock(app).start_pending);
    emit_state(app);
    if pending {
        start_recording(app);
    }
}

/// Остановить захват. Дописывание буфера и файла записи идёт в фоне: команды интерфейса
/// и меню трея выполняются в главном потоке, и окно, трей и клавиши замирали бы на секунды.
fn stop_recording(app: &AppHandle) {
    stop_recording_inner(app, false);
}

/// То же, но дождаться конца (сон и выключение Windows).
fn stop_recording_wait(app: &AppHandle) {
    stop_recording_inner(app, true);
}

fn stop_recording_inner(app: &AppHandle, wait: bool) {
    let rec = {
        let mut i = lock(app);
        i.status = None;
        i.capture_blocked = None;
        i.record_started = None;
        i.start_pending = false;
        let rec = i.recorder.take();
        if rec.is_some() {
            i.stopping = true;
        }
        rec
    };
    st(app).generation.fetch_add(1, Ordering::SeqCst);
    show_indicator(app, false);
    emit_state(app);
    let Some(rec) = rec else {
        if wait {
            wait_stopper(app);
        }
        return;
    };
    let a = app.clone();
    let finish = move || {
        // остановка ждёт поток захвата (и дописывание файла записи) — вне блокировки
        drop(rec);
        log("запись остановлена");
        let pending = {
            let mut i = lock(&a);
            i.stopping = false;
            std::mem::take(&mut i.start_pending)
        };
        clips_changed(&a);
        emit_state(&a);
        if pending {
            start_recording(&a);
        }
    };
    if wait {
        wait_stopper(app);
        finish();
    } else {
        // прошлой остановки здесь быть не может: пока она идёт, захват не запускается
        let h = std::thread::spawn(finish);
        *st(app).stopper.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }
}

/// Дождаться фоновой остановки (перед сном, выходом и обновлением файл записи должен дописаться).
fn wait_stopper(app: &AppHandle) {
    let h = st(app).stopper.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(h) = h {
        let _ = h.join();
    }
}

fn restart_recording(app: &AppHandle) {
    stop_recording(app);
    start_recording(app);
}

fn forward_events(app: AppHandle, events: mpsc::Receiver<Event>, generation: u64) {
    for ev in events.iter() {
        if st(&app).generation.load(Ordering::SeqCst) != generation {
            return;
        }
        match ev {
            Event::Status { buffered_secs, buffer_bytes, fps, dropped } => {
                lock(&app).status = Some(LiveStatus { buffered_secs, buffer_bytes, fps, dropped });
                emit_status(&app);
                storage::on_status(&app);
            }
            Event::Saved { path, seconds, bytes } => {
                log(format!("сохранён повтор {:.1} с, {} байт: {}", seconds, bytes, path.display()));
                clips_changed(&app);
                let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let _ = app.emit("clip-saved", path.to_string_lossy().to_string());
                cue(&app, sound::Cue::Saved);
                auto_clean_later(&app);
                show_toast(&app, "Повтор сохранён", &format!("{} · {}", fmt_secs(seconds), name), "replay");
            }
            Event::SaveFailed { message } => {
                log(format!("не удалось сохранить: {message}"));
                show_toast(&app, "Не удалось сохранить", &message, "error");
            }
            Event::RecordStarted { path } => {
                log(format!("запись в файл: {}", path.display()));
                let key = {
                    let mut i = lock(&app);
                    i.record_started = Some(now_ms());
                    i.settings.hotkey_record.clone()
                };
                emit_state(&app);
                show_indicator(&app, true);
                cue(&app, sound::Cue::RecordStart);
                show_toast(&app, "Запись началась", &format!("{key} — остановить"), "rec");
            }
            Event::Recorded { path, seconds, bytes } => {
                log(format!("запись сохранена {:.1} с, {} байт: {}", seconds, bytes, path.display()));
                lock(&app).record_started = None;
                show_indicator(&app, false);
                emit_state(&app);
                clips_changed(&app);
                let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let _ = app.emit("clip-saved", path.to_string_lossy().to_string());
                cue(&app, sound::Cue::RecordStop);
                auto_clean_later(&app);
                let title = if storage::take_low_stop() { "Мало места — запись сохранена" } else { "Запись сохранена" };
                show_toast(&app, title, &format!("{} · {}", fmt_secs(seconds), name), "rec");
            }
            Event::RecordFailed { message } => {
                log(format!("запись не удалась: {message}"));
                storage::take_low_stop();
                lock(&app).record_started = None;
                show_indicator(&app, false);
                emit_state(&app);
                show_toast(&app, "Запись не удалась", &message, "error");
            }
            Event::CaptureBlocked { game, exe } => {
                let nvidia = lock(&app).capture.as_ref().is_some_and(|c| c.gpu.to_uppercase().contains("NVIDIA"));
                // настройка уже включена (игру не перезапускали) — только чтение настроек драйвера
                let driver_ok = nvidia && exe.as_deref().is_some_and(|e| matches!(nvidia::present_method(e), Ok(nvidia::PREFER_DXGI)));
                log(format!("захват: «{game}» ({}) не попадает в запись; видеокарта NVIDIA: {nvidia}, настройка DXGI уже включена: {driver_ok}", exe.as_deref().unwrap_or("?")));
                let first = {
                    let mut i = lock(&app);
                    i.capture_blocked = Some(BlockedGame { game: game.clone(), exe, nvidia, driver_ok });
                    i.blocked_toasted.insert(game.clone())
                };
                emit_state(&app);
                if first {
                    show_toast(&app, &format!("{game} не записывается"), "Как исправить — в окне Clipback", "error");
                }
            }
            Event::CaptureOk => {
                lock(&app).capture_blocked = None;
                emit_state(&app);
            }
            Event::Error { message } => {
                log(format!("ошибка записи: {message}"));
                let rec = {
                    let mut i = lock(&app);
                    i.error = Some(message.clone());
                    i.status = None;
                    i.capture_blocked = None;
                    i.record_started = None;
                    i.recorder.take()
                };
                show_indicator(&app, false);
                drop(rec);
                emit_state(&app);
                show_toast(&app, "Запись остановилась", &format!("{message}. Перезапускаю…"), "error");
                retry_recording_later(&app, generation);
                return;
            }
            Event::Started { .. } => {}
        }
    }
    // поток захвата завершился, ничего не сказав (паника — её текст уже в журнале):
    // без этого интерфейс показывал бы, что запись идёт, а буфер бы не пополнялся
    if st(&app).generation.load(Ordering::SeqCst) == generation {
        log("ошибка записи: поток захвата неожиданно завершился");
        let rec = {
            let mut i = lock(&app);
            i.error = Some("поток захвата неожиданно завершился".into());
            i.status = None;
            i.capture_blocked = None;
            i.record_started = None;
            i.recorder.take()
        };
        show_indicator(&app, false);
        drop(rec);
        emit_state(&app);
        show_toast(&app, "Запись остановилась", "Поток захвата неожиданно завершился. Перезапускаю…", "error");
        retry_recording_later(&app, generation);
    }
}

fn fmt_secs(s: f64) -> String {
    let s = s.round() as u64;
    if s >= 3600 {
        return format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    }
    format!("{}:{:02}", s / 60, s % 60)
}

/// Значок записи в углу: окно живёт в главном потоке программы.
fn show_indicator(app: &AppHandle, on: bool) {
    let on = on && lock(app).settings.record_indicator;
    if let Err(e) = app.run_on_main_thread(move || indicator::set_visible(on)) {
        log(format!("значок записи: {e}"));
    }
}

/// Запись автозапуска в Windows — по настройке. Проверяется при каждом старте:
/// путь к программе мог измениться (обновление, перенос portable-версии).
fn sync_autostart(app: &AppHandle) {
    let want = lock(app).settings.autostart;
    let al = app.autolaunch();
    let r = if want { al.enable() } else if al.is_enabled().unwrap_or(false) { al.disable() } else { Ok(()) };
    if let Err(e) = r {
        log(format!("автозапуск: {e}"));
    }
}

fn cue(app: &AppHandle, c: sound::Cue) {
    if lock(app).settings.save_sound {
        sound::play(c);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Начать или остановить полноценную запись. Если захват выключен — включить его и начать.
fn toggle_record(app: &AppHandle) {
    let label = current_game_label(app);
    if lock(app).record_started.is_none() {
        if let Some(r) = storage::record_blocked(app) {
            show_toast(app, &r, "Запись не начата", "error");
            return;
        }
    }
    let mut i = lock(app);
    if let Some(r) = &i.recorder {
        let dir = Some(util::clip_dir(&i.settings.out_dir, label.as_deref(), i.settings.game_folders));
        let cmd = if i.record_started.is_some() { Command::StopRecord } else { Command::StartRecord { label, dir } };
        let _ = r.sender().send(cmd);
    } else if i.starting {
        i.record_after_start = !i.record_after_start;
    } else {
        i.record_after_start = true;
        drop(i);
        start_recording(app);
    }
}

/// Название игры для имени файла: окно в фокусе, если это не сам Clipback.
fn current_game_label(app: &AppHandle) -> Option<String> {
    if let Some((title, pid)) = util::foreground_window() {
        if pid != std::process::id() {
            if let Some(l) = util::clip_label(&title) {
                lock(app).last_game = Some(l.clone());
                return Some(l);
            }
        }
    }
    lock(app).last_game.clone()
}

fn save_replay(app: &AppHandle) {
    let label = current_game_label(app);
    if lock(app).recorder.is_some() {
        if let Some(r) = storage::save_blocked(app, storage::replay_expected(app)) {
            show_toast(app, &r, "Клип не сохранён — повтор пока в памяти", "error");
            return;
        }
    }
    let i = lock(app);
    match &i.recorder {
        Some(r) => {
            let dir = Some(util::clip_dir(&i.settings.out_dir, label.as_deref(), i.settings.game_folders));
            let _ = r.sender().send(Command::Save { seconds: None, label, dir });
        }
        None => {
            drop(i);
            show_toast(app, "Запись выключена", "Включите запись буфера в Clipback", "error");
        }
    }
}

fn take_screenshot(app: &AppHandle) -> Result<clips::ClipEntry, String> {
    let game = current_game_label(app);
    let label = game.clone().unwrap_or_else(|| "Скриншот".into());
    storage::ensure_space(app, storage::shot_expected(app), "скриншот не сохранён")?;
    let (tx, rx) = mpsc::channel();
    let (dir, jpeg) = {
        let i = lock(app);
        let r = i.recorder.as_ref().ok_or("запись выключена — скриншот берётся из захвата экрана")?;
        r.sender().send(Command::Screenshot { reply: tx }).map_err(|e| e.to_string())?;
        (util::clip_dir(&i.settings.out_dir, game.as_deref(), i.settings.game_folders), i.settings.screenshot_format != "png")
    };
    let raw = rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "захват не ответил".to_string())??;
    // сразу в буфер обмена — можно вставить в чат, не открывая Clipback
    if lock(app).settings.screenshot_clipboard {
        if let Err(e) = winutil::copy_image_to_clipboard(&raw.rgba, raw.width, raw.height) {
            log(format!("скриншот в буфер: {e:#}"));
        }
    }
    let (data, _) = clipback_core::shot::encode(&raw, jpeg).map_err(|e| format!("{e:#}"))?;
    let e = clips::save_image(&dir, &label, &data).map_err(|e| format!("{e:#}"))?;
    log(format!("скриншот: {}", e.path));
    clips_changed(app);
    Ok(e)
}

// ---------- окна ----------

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    // пока окно было спрятано, тики буфера и список клипов ему не обновлялись
    let _ = app.emit_to("main", "main-shown", ());
    emit_state(app);
}

/// Спрятать оверлей: сначала интерфейс плавно гаснет, потом окно скрывается.
fn hide_overlay(app: &AppHandle) {
    let Some(w) = app.get_webview_window("overlay") else { return };
    if !w.is_visible().unwrap_or(false) {
        return;
    }
    let seq = st(app).overlay_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = app.emit_to("overlay", "overlay-hide", ());
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(170));
        // за это время оверлей могли открыть снова — тогда не прячем
        if st(&app).overlay_seq.load(Ordering::SeqCst) == seq {
            if let Some(w) = app.get_webview_window("overlay") {
                let _ = w.hide();
            }
            let _ = app.emit_to("overlay", "overlay-hidden", ());
        }
    });
}

fn toggle_overlay(app: &AppHandle) {
    let Some(w) = app.get_webview_window("overlay") else { return };
    if w.is_visible().unwrap_or(false) {
        hide_overlay(app);
        return;
    }
    let _ = current_game_label(app);
    // растягиваем на весь основной монитор
    if let Ok(Some(m)) = w.primary_monitor() {
        let _ = w.set_position(PhysicalPosition::new(m.position().x, m.position().y));
        let _ = w.set_size(PhysicalSize::new(m.size().width, m.size().height));
    }
    st(app).overlay_seq.fetch_add(1, Ordering::SeqCst);
    // интерфейс оверлея начинает прозрачным и плавно проявляется (см. overlay-shown)
    let _ = app.emit_to("overlay", "overlay-will-show", ());
    let _ = w.show();
    let _ = w.set_always_on_top(true);
    let _ = w.set_focus();
    let _ = app.emit_to("overlay", "overlay-shown", ());
    emit_state(app);
}

#[derive(Serialize, Clone)]
struct ToastDto {
    title: String,
    sub: String,
    icon: String,
}

/// Уведомление в углу экрана. Не забирает фокус — игра не свернётся.
fn show_toast(app: &AppHandle, title: &str, sub: &str, icon: &str) {
    let Some(w) = app.get_webview_window("toast") else { return };
    let seq = st(app).toast_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = app.emit_to("toast", "toast", ToastDto { title: title.into(), sub: sub.into(), icon: icon.into() });
    if let Ok(Some(m)) = w.primary_monitor() {
        let scale = m.scale_factor();
        let (tw, margin) = ((360.0 * scale) as i32, (20.0 * scale) as i32);
        let _ = w.set_position(PhysicalPosition::new(m.position().x + m.size().width as i32 - tw - margin, m.position().y + margin));
    }
    if let Ok(h) = w.hwnd() {
        winutil::show_no_activate(h.0 as isize);
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(3200));
        // скрываем, только если за это время не появилось новое уведомление
        if st(&app).toast_seq.load(Ordering::SeqCst) == seq {
            if let Some(w) = app.get_webview_window("toast") {
                if let Ok(h) = w.hwnd() {
                    winutil::hide(h.0 as isize);
                }
            }
        }
    });
}

fn create_windows(app: &AppHandle, start_hidden: bool) -> tauri::Result<()> {
    let main = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Clipback")
        .inner_size(1320.0, 820.0)
        .min_inner_size(980.0, 620.0)
        .theme(Some(tauri::Theme::Dark))
        .background_color(tauri::window::Color(17, 18, 20, 255))
        .initialization_script("window.__CLIPBACK_MODE__ = 'window';")
        .visible(!start_hidden)
        .center()
        .build()?;
    let handle = app.clone();
    main.on_window_event(move |e| match e {
        WindowEvent::CloseRequested { api, .. } => {
            if lock(&handle).settings.close_to_tray {
                api.prevent_close();
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
                // спрятанное окно WebView2 не считает скрытым — видео продолжало бы играть
                let _ = handle.emit_to("main", "main-hidden", ());
            } else {
                handle.exit(0);
            }
        }
        WindowEvent::Resized(_) => {
            if handle.get_webview_window("main").is_some_and(|w| w.is_minimized().unwrap_or(false)) {
                let _ = handle.emit_to("main", "main-hidden", ());
            }
        }
        _ => {}
    });

    let overlay = WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("index.html".into()))
        .title("Clipback — оверлей")
        .initialization_script("window.__CLIPBACK_MODE__ = 'overlay';")
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .visible(false)
        .inner_size(1280.0, 720.0)
        .build()?;
    let handle = app.clone();
    overlay.on_window_event(move |e| {
        if let WindowEvent::CloseRequested { api, .. } = e {
            api.prevent_close();
            hide_overlay(&handle);
        }
    });

    let toast = WebviewWindowBuilder::new(app, "toast", WebviewUrl::App("index.html".into()))
        .title("Clipback — уведомление")
        .initialization_script("window.__CLIPBACK_MODE__ = 'toast';")
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .focusable(false)
        .visible(false)
        .position(-10000.0, -10000.0)
        .inner_size(360.0, 80.0)
        .build()?;
    // окно без рамки получает свой окончательный стиль при первом показе через Tauri;
    // показываем его один раз за краем экрана (без фокуса — так создано), затем прячем
    let _ = toast.show();
    let _ = toast.hide();
    if let Ok(h) = toast.hwnd() {
        winutil::make_passive(h.0 as isize);
        indicator::exclude_from_capture(h.0 as isize);
    }
    // оверлей не должен попадать в повтор и скриншоты
    if let Ok(h) = overlay.hwnd() {
        indicator::exclude_from_capture(h.0 as isize);
    }
    Ok(())
}

// ---------- трей ----------

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Открыть Clipback", true, None::<&str>)?;
    let save = MenuItem::with_id(app, "save", "Сохранить повтор", true, None::<&str>)?;
    let record = MenuItem::with_id(app, "record", "Начать запись", true, None::<&str>)?;
    let rec = CheckMenuItem::with_id(app, "rec", "Запись буфера", true, true, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "Открыть папку клипов", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &save, &record, &sep, &rec, &folder, &sep2, &quit])?;
    *st(app).rec_item.lock().unwrap() = Some(rec);
    *st(app).record_item.lock().unwrap() = Some(record);

    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().cloned().expect("нет иконки"))
        .tooltip("Clipback")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| match e.id().as_ref() {
            "open" => show_main(app),
            "save" => save_replay(app),
            "record" => toggle_record(app),
            "rec" => {
                if lock(app).recorder.is_some() {
                    stop_recording(app)
                } else {
                    start_recording(app)
                }
            }
            "folder" => {
                let dir = lock(app).settings.out_dir.clone();
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn update_tray(app: &AppHandle, d: &StateDto) {
    // меню и подсказку трея трогаем только когда они меняются
    static LAST: Mutex<Option<(bool, bool, String)>> = Mutex::new(None);
    let Some(tray) = app.tray_by_id("main") else { return };
    let tip = match (&d.status, d.recording) {
        _ if d.record_started.is_some() => "Clipback — идёт запись".to_string(),
        (Some(s), true) => format!("Clipback — в буфере {}", fmt_secs(s.buffered_secs)),
        (_, true) => "Clipback — запись идёт".to_string(),
        _ => "Clipback — запись выключена".to_string(),
    };
    let now = (d.recording || d.starting, d.record_started.is_some(), tip);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_ref() == Some(&now) {
        return;
    }
    if let Some(item) = st(app).rec_item.lock().ok().and_then(|g| g.clone()) {
        let _ = item.set_checked(now.0);
    }
    if let Some(item) = st(app).record_item.lock().ok().and_then(|g| g.clone()) {
        let _ = item.set_text(if now.1 { "Остановить запись" } else { "Начать запись" });
    }
    let _ = tray.set_tooltip(Some(now.2.clone()));
    *last = Some(now);
}

// ---------- горячие клавиши ----------

fn register_hotkeys(app: &AppHandle) {
    let s = lock(app).settings.clone();
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let mut map = Vec::new();
    let mut errors = Vec::new();
    for (combo, action, what) in [
        (&s.hotkey_overlay, Action::Overlay, "оверлей"),
        (&s.hotkey_save, Action::Save, "сохранение"),
        (&s.hotkey_screenshot, Action::Screenshot, "скриншот"),
        (&s.hotkey_record, Action::Record, "запись"),
    ] {
        match combo.parse::<Shortcut>() {
            Ok(sc) => match gs.register(sc) {
                Ok(()) => map.push((sc, action)),
                Err(e) => errors.push(format!("{combo} ({what}) занята другой программой: {e}")),
            },
            Err(e) => errors.push(format!("{combo} ({what}): {e}")),
        }
    }
    for e in &errors {
        log(format!("горячая клавиша: {e}"));
    }
    let mut i = lock(app);
    i.hotkeys = map;
    i.hotkey_errors = errors;
}

fn on_hotkey(app: &AppHandle, sc: &Shortcut) {
    let action = lock(app).hotkeys.iter().find(|(s, _)| s.id() == sc.id()).map(|(_, a)| *a);
    match action {
        Some(Action::Overlay) => toggle_overlay(app),
        Some(Action::Save) => save_replay(app),
        Some(Action::Record) => toggle_record(app),
        Some(Action::Screenshot) => {
            let app = app.clone();
            std::thread::spawn(move || match take_screenshot(&app) {
                Ok(e) => {
                    cue(&app, sound::Cue::Saved);
                    let sub = if lock(&app).settings.screenshot_clipboard { "И скопирован — Ctrl+V вставит его в чат" } else { e.name.as_str() };
                    show_toast(&app, "Скриншот сохранён", sub, "camera")
                }
                Err(e) => show_toast(&app, "Скриншот не получился", &e, "error"),
            });
        }
        None => {}
    }
}

// ---------- команды для интерфейса ----------

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn out_dir(app: &AppHandle) -> PathBuf {
    lock(app).settings.out_dir.clone()
}

fn checked_path(app: &AppHandle, path: &str) -> Res<PathBuf> {
    clips::ensure_inside(&out_dir(app), Path::new(path)).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn get_state(app: AppHandle) -> StateDto {
    dto(&lock(&app))
}

#[tauri::command]
fn set_recording(app: AppHandle, on: bool) {
    if on {
        start_recording(&app)
    } else {
        stop_recording(&app)
    }
}

#[tauri::command]
fn save_replay_cmd(app: AppHandle) {
    save_replay(&app)
}

#[tauri::command]
fn toggle_record_cmd(app: AppHandle) {
    toggle_record(&app)
}

/// Долгая работа (диск, перекодирование, COM) — в отдельном потоке: иначе она занимает
/// поток асинхронных команд, и опрос состояния и микшера ждёт, пока идёт экспорт.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn screenshot(app: AppHandle) -> Res<clips::ClipEntry> {
    blocking(move || take_screenshot(&app)).await
}

#[tauri::command]
async fn list_clips(app: AppHandle) -> Vec<clips::ClipEntry> {
    storage::recheck(&app);
    let dir = out_dir(&app);
    let mut l = clips::list(&dir);
    favs(&app).mark(&mut l);
    hidden(&app).mark_with(&mut l, |e| &mut e.hidden);
    l
}

fn favs(app: &AppHandle) -> std::sync::MutexGuard<'_, library::Favorites> {
    st(app).favorites.lock().unwrap_or_else(|e| e.into_inner())
}

fn hidden(app: &AppHandle) -> std::sync::MutexGuard<'_, library::Favorites> {
    st(app).hidden.lock().unwrap_or_else(|e| e.into_inner())
}

/// Метка «скрытая запись»: превью размыто, открыть можно только после подтверждения.
#[tauri::command]
fn set_hidden(app: AppHandle, path: String, on: bool) -> Res<()> {
    let p = checked_path(&app, &path)?;
    hidden(&app).set(&p, on);
    clips_changed(&app);
    Ok(())
}

/// Новый файл из скрытой записи (отрезок, кадр, правка) тоже скрыт.
fn inherit_hidden(app: &AppHandle, from: &Path, e: &mut clips::ClipEntry) {
    let mut h = hidden(app);
    if h.contains(from) {
        h.set(Path::new(&e.path), true);
        e.hidden = true;
    }
}

#[tauri::command]
fn set_favorite(app: AppHandle, path: String, on: bool) -> Res<()> {
    let p = checked_path(&app, &path)?;
    favs(&app).set(&p, on);
    clips_changed(&app);
    Ok(())
}

/// Автоочистка: самые старые клипы — в корзину, пока папка не уложится в лимиты.
fn auto_clean(app: &AppHandle) {
    // запуски после сохранения, смены настроек и раз в 30 мин не должны идти одновременно:
    // второй убирал бы те же файлы (ошибки, двойное уведомление)
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    auto_clean_once(app);
    RUNNING.store(false, Ordering::SeqCst);
}

fn auto_clean_once(app: &AppHandle) {
    let s = lock(app).settings.clone();
    if !s.auto_clean || (s.clean_max_gb == 0 && s.clean_max_days == 0) {
        return;
    }
    let mut l = clips::list(&s.out_dir);
    favs(app).mark(&mut l);
    let max_bytes = (s.clean_max_gb > 0).then(|| s.clean_max_gb as u64 * 1_000_000_000);
    let max_days = (s.clean_max_days > 0).then_some(s.clean_max_days);
    let plan = library::clean_plan(&l, max_bytes, max_days, now_ms());
    if plan.is_empty() {
        return;
    }
    let (mut n, mut bytes) = (0, 0u64);
    for p in &plan {
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        match winutil::move_to_recycle_bin(p) {
            Ok(()) => {
                n += 1;
                bytes += size;
                log(format!("автоочистка: в корзину {}", p.display()));
            }
            Err(e) => log(format!("автоочистка: {} — {e:#}", p.display())),
        }
    }
    if n > 0 {
        clips_changed(app);
        show_toast(app, "Автоочистка", &format!("В корзину: {n} шт., {:.1} ГБ", bytes as f64 / 1e9), "trash");
    }
}

#[derive(Serialize)]
struct CleanPreview {
    count: usize,
    bytes: u64,
}

/// Сколько клипов автоочистка уберёт прямо сейчас при таких лимитах — для подтверждения в настройках.
#[tauri::command]
async fn clean_preview(app: AppHandle, max_gb: u32, max_days: u32) -> CleanPreview {
    let dir = out_dir(&app);
    let mut l = clips::list(&dir);
    favs(&app).mark(&mut l);
    let plan = library::clean_plan(&l, (max_gb > 0).then(|| max_gb as u64 * 1_000_000_000), (max_days > 0).then_some(max_days), now_ms());
    let bytes = l.iter().filter(|e| plan.iter().any(|p| Path::new(&e.path) == p)).map(|e| e.size).sum();
    CleanPreview { count: plan.len(), bytes }
}

fn auto_clean_later(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || auto_clean(&app));
}

/// Включить для игры, которая не попадает в захват, настройку NVIDIA «Использовать цепочку
/// буферов DXGI» — только для её программы. Windows может спросить права администратора.
#[tauri::command]
async fn fix_capture_blocked(app: AppHandle) -> Res<()> {
    let exe = lock(&app).capture_blocked.as_ref().and_then(|b| b.exe.clone()).ok_or("не знаю, какая программа у игры")?;
    let r = tauri::async_runtime::spawn_blocking({
        let exe = exe.clone();
        move || nvidia::enable_dxgi_elevating(&exe)
    })
    .await
    .map_err(|e| e.to_string())?;
    match r {
        Ok(()) => {
            log(format!("настройка NVIDIA «цепочка буферов DXGI» включена для {exe}"));
            if let Some(b) = lock(&app).capture_blocked.as_mut() {
                b.driver_ok = true;
            }
            emit_state(&app);
            Ok(())
        }
        Err(e) => {
            log(format!("настройка NVIDIA для {exe}: {e}"));
            Err(e)
        }
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn trim_clip(
    app: AppHandle,
    path: String,
    start: f64,
    end: f64,
    replace: bool,
    audio_keep: Option<Vec<usize>>,
    // громкость каждой дорожки файла (1 — как есть); None — не менялась
    audio_gains: Option<Vec<f32>>,
    quality: Option<String>,
) -> Res<clips::TrimOut> {
    blocking(move || trim_clip_now(app, path, start, end, replace, audio_keep, audio_gains, quality)).await
}

#[allow(clippy::too_many_arguments)]
fn trim_clip_now(
    app: AppHandle,
    path: String,
    start: f64,
    end: f64,
    replace: bool,
    audio_keep: Option<Vec<usize>>,
    audio_gains: Option<Vec<f32>>,
    quality: Option<String>,
) -> Res<clips::TrimOut> {
    let p = checked_path(&app, &path)?;
    let q = quality.filter(|q| q != "original");
    storage::ensure_space(&app, storage::trim_expected_for(&p, start, end, q.as_deref()), "отрезок не сохранён")?;
    let progress_app = app.clone();
    let mut last = 0.0;
    let mut progress = move |f: f64| {
        // не чаще чем на 2 % — интерфейсу хватает
        if f - last >= 0.02 || f >= 1.0 {
            last = f;
            let _ = progress_app.emit("export-progress", f);
        }
    };
    let mut r = clips::trim(&p, start, end, replace, audio_keep.as_deref(), audio_gains.as_deref(), q.as_deref(), &mut progress).map_err(|e| {
        log(format!("не удалось сохранить отрезок: {e:#}"));
        format!("{e:#}")
    })?;
    inherit_hidden(&app, &p, &mut r.clip);
    log(format!(
        "отрезок {:.2}–{:.2} с ({}, качество {}): {}",
        r.start,
        r.end,
        if replace { "замена" } else { "копия" },
        q.as_deref().unwrap_or("оригинал"),
        r.clip.path
    ));
    clips_changed(&app);
    Ok(r)
}

#[tauri::command]
async fn rename_clip(app: AppHandle, path: String, name: String) -> Res<clips::ClipEntry> {
    let p = checked_path(&app, &path)?;
    let e = clips::rename(&p, &name).map_err(|e| format!("{e:#}"))?;
    favs(&app).renamed(&p, Path::new(&e.path));
    hidden(&app).renamed(&p, Path::new(&e.path));
    clips_changed(&app);
    Ok(e)
}

#[tauri::command]
async fn delete_clip(app: AppHandle, path: String) -> Res<DeleteOut> {
    delete_clips(app, vec![path]).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteOut {
    /// Номер пачки — для «Вернуть».
    batch: u64,
    count: usize,
    bytes: u64,
    errors: Vec<String>,
}

/// В корзину сразу несколько клипов. Несколько секунд их можно вернуть (undo_delete),
/// потом они уходят в корзину Windows.
#[tauri::command]
async fn delete_clips(app: AppHandle, paths: Vec<String>) -> Res<DeleteOut> {
    let ps: Vec<PathBuf> = paths.iter().map(|p| checked_path(&app, p)).collect::<Res<_>>()?;
    let bytes = ps.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    let (batch, done, errors) = st(&app).trash.stage(&ps);
    for e in &errors {
        log(format!("не удалось убрать: {e}"));
    }
    if done.is_empty() {
        return Err(errors.first().cloned().unwrap_or_else(|| "нечего удалять".into()));
    }
    log(format!("в корзину ({} шт., можно вернуть {} с)", done.len(), trash::UNDO_SECS));
    clips_changed(&app);
    let a = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(trash::UNDO_SECS));
        let gone = st(&a).trash.finalize(batch);
        for p in &gone {
            favs(&a).set(p, false);
            hidden(&a).set(p, false);
        }
        if !gone.is_empty() {
            clips_changed(&a);
        }
    });
    Ok(DeleteOut { batch, count: done.len(), bytes, errors })
}

/// «Вернуть» — пока не истекло время.
#[tauri::command]
fn undo_delete(app: AppHandle, batch: u64) -> Res<Vec<String>> {
    let back = st(&app).trash.restore(batch);
    if back.is_empty() {
        return Err("уже поздно — клипы в корзине Windows, их можно вернуть оттуда".into());
    }
    log(format!("возвращено из корзины: {} шт.", back.len()));
    clips_changed(&app);
    Ok(back.into_iter().map(|p| p.to_string_lossy().into_owned()).collect())
}

#[tauri::command]
async fn reveal_clip(app: AppHandle, window: tauri::Window, path: String) -> Res<()> {
    let p = checked_path(&app, &path)?;
    leave_overlay_for_explorer(&app, &window);
    winutil::reveal_in_explorer(&p).map_err(|e| format!("{e:#}"))
}

/// Проводник открывается под оверлеем (тот всегда сверху) — поэтому оверлей сначала убираем.
fn leave_overlay_for_explorer(app: &AppHandle, window: &tauri::Window) {
    if window.label() == "overlay" {
        hide_overlay(app);
    }
}

#[tauri::command]
fn open_clips_folder(app: AppHandle, window: tauri::Window) -> Res<()> {
    let dir = out_dir(&app);
    leave_overlay_for_explorer(&app, &window);
    std::fs::create_dir_all(&dir).map_err(err)?;
    std::process::Command::new("explorer").arg(&dir).spawn().map_err(err)?;
    Ok(())
}

/// Картинка (PNG от интерфейса) — в буфер обмена как изображение.
#[tauri::command]
async fn copy_image(png_base64: String) -> Res<()> {
    let data = base64::engine::general_purpose::STANDARD.decode(png_base64.as_bytes()).map_err(err)?;
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().map_err(err)?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).map_err(err)?;
    let px = &buf[..info.buffer_size()];
    // в RGBA, какой бы ни была картинка
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
    };
    winutil::copy_image_to_clipboard(&rgba, info.width, info.height).map_err(|e| format!("{e:#}"))
}

/// Перетаскивание клипа из Clipback в другую программу (Discord, Telegram, проводник).
/// В оверлее окно на время перетаскивания прячется, чтобы было куда бросить;
/// если клип никуда не бросили — оверлей возвращается.
#[tauri::command]
fn start_drag(app: AppHandle, window: tauri::Window, path: String, icon_png_base64: Option<String>) -> Res<()> {
    let p = checked_path(&app, &path)?;
    let icon = icon_png_base64
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b.as_bytes()).ok())
        .filter(|b| b.starts_with(&[0x89, b'P', b'N', b'G']))
        .unwrap_or_else(|| TRANSPARENT_PNG.to_vec());
    let label = window.label().to_string();
    let from_overlay = label == "overlay";
    if from_overlay {
        if let Some(w) = app.get_webview_window("overlay") {
            let _ = w.hide();
        }
    }
    let a = app.clone();
    app.run_on_main_thread(move || {
        let Some(w) = a.get_webview_window(&label) else { return };
        let a2 = a.clone();
        let r = drag::start_drag(
            &w,
            drag::DragItem::Files(vec![p]),
            drag::Image::Raw(icon),
            move |result, _| {
                let dropped = matches!(result, drag::DragResult::Dropped);
                if from_overlay && !dropped {
                    toggle_overlay(&a2);
                }
                let _ = a2.emit("drag-done", dropped);
            },
            drag::Options::default(),
        );
        if let Err(e) = r {
            log(format!("перетаскивание: {e}"));
            if from_overlay {
                toggle_overlay(&a);
            }
        }
    })
    .map_err(err)
}

/// Прозрачная картинка 1×1 — если интерфейс не прислал миниатюру.
const TRANSPARENT_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01,
    0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[tauri::command]
async fn copy_clip(app: AppHandle, path: String) -> Res<()> {
    let p = checked_path(&app, &path)?;
    winutil::copy_file_to_clipboard(&p).map_err(|e| format!("{e:#}"))
}

/// PNG кадра из плеера (интерфейс рисует кадр в canvas и присылает base64).
#[tauri::command]
async fn save_frame(app: AppHandle, png_base64: String, label: Option<String>, from: Option<String>) -> Res<clips::ClipEntry> {
    let data = base64::engine::general_purpose::STANDARD.decode(png_base64.as_bytes()).map_err(err)?;
    let label = label.and_then(|l| clips::clean_name(&l).ok()).unwrap_or_else(|| "Кадр".into());
    storage::ensure_space(&app, data.len() as u64, "кадр не сохранён")?;
    // кадр — рядом с клипом, из которого он взят (в той же папке игры)
    let src = from.and_then(|f| checked_path(&app, &f).ok());
    let dir = src.as_deref().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_else(|| out_dir(&app));
    let mut e = clips::save_image(&dir, &label, &data).map_err(|e| format!("{e:#}"))?;
    if let Some(src) = src {
        inherit_hidden(&app, &src, &mut e);
    }
    clips_changed(&app);
    Ok(e)
}

#[tauri::command]
async fn save_edited_image(app: AppHandle, path: String, png_base64: String, replace: bool) -> Res<clips::ClipEntry> {
    let p = checked_path(&app, &path)?;
    let data = base64::engine::general_purpose::STANDARD.decode(png_base64.as_bytes()).map_err(err)?;
    storage::ensure_space(&app, data.len() as u64, "скриншот не сохранён")?;
    let mut e = clips::save_edited(&p, &data, replace).map_err(|e| format!("{e:#}"))?;
    inherit_hidden(&app, &p, &mut e);
    log(format!("скриншот отредактирован: {}", e.path));
    clips_changed(&app);
    Ok(e)
}

/// Новая папка клипов: полный путь, создаётся и в неё можно писать.
fn check_out_dir(dir: &Path) -> Res<()> {
    if dir.as_os_str().is_empty() || !dir.is_absolute() {
        return Err("папка клипов: укажите полный путь, например D:\\Клипы".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("папка клипов не создаётся: {e}"))?;
    let probe = dir.join(".clipback-write-test");
    std::fs::write(&probe, b"ok").map_err(|e| format!("в папку клипов нельзя записывать: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) -> Res<Settings> {
    let mut new = settings.sanitized();
    let path = st(&app).settings_path.clone();
    {
        let i = lock(&app);
        // замки громкости меняет только микшер (widgets.rs): окно настроек могло прислать старые
        new.volume_locks = i.settings.volume_locks.clone();
        if i.settings.out_dir != new.out_dir {
            drop(i);
            check_out_dir(&new.out_dir)?;
        }
    }
    // сначала на диск: не сохранилось — программа остаётся со старыми настройками
    new.save(&path).map_err(|e| format!("не удалось сохранить настройки: {e:#}"))?;
    let old = std::mem::replace(&mut lock(&app).settings, new.clone());

    if old.out_dir != new.out_dir {
        let _ = std::fs::create_dir_all(&new.out_dir);
        let _ = app.asset_protocol_scope().allow_directory(&new.out_dir, true);
        storage::recheck(&app);
        clips_changed(&app);
        let dir = new.out_dir.clone();
        std::thread::spawn(move || trash::flush_leftovers(&dir));
    }
    if (&old.hotkey_overlay, &old.hotkey_save, &old.hotkey_screenshot, &old.hotkey_record)
        != (&new.hotkey_overlay, &new.hotkey_save, &new.hotkey_screenshot, &new.hotkey_record)
    {
        register_hotkeys(&app);
    }
    if new.auto_clean && (!old.auto_clean || old.clean_max_gb != new.clean_max_gb || old.clean_max_days != new.clean_max_days) {
        auto_clean_later(&app);
    }
    if old.autostart != new.autostart {
        let al = app.autolaunch();
        let r = if new.autostart { al.enable() } else { al.disable() };
        if let Err(e) = r {
            log(format!("автозапуск: {e}"));
        }
    }
    let recording = lock(&app).recorder.is_some();
    if recording && old.capture_changed(&new) {
        restart_recording(&app);
    } else if recording && old.buffer_minutes != new.buffer_minutes {
        if let Some(r) = &lock(&app).recorder {
            let _ = r.sender().send(Command::SetBufferLength((new.buffer_minutes * 60.0).round() as u32));
        }
    }
    emit_state(&app);
    Ok(new)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MonitorDto {
    index: usize,
    name: String,
    width: u32,
    height: u32,
}

#[tauri::command]
fn list_monitors() -> Vec<MonitorDto> {
    gpu::list_monitors()
        .unwrap_or_default()
        .into_iter()
        .map(|(index, name, width, height)| MonitorDto { index, name: name.trim_start_matches(r"\\.\").to_string(), width, height })
        .collect()
}

#[tauri::command]
async fn list_audio_apps() -> Res<Vec<clipback_core::audio::AudioApp>> {
    blocking(|| clipback_core::audio::list_audio_apps().map_err(|e| format!("{e:#}"))).await
}

/// Форма волны звуковых дорожек клипа (считается один раз и запоминается).
#[tauri::command]
async fn audio_peaks(app: AppHandle, path: String, bins: usize) -> Res<Vec<Vec<f32>>> {
    blocking(move || audio_peaks_now(&app, &path, bins)).await
}

fn audio_peaks_now(app: &AppHandle, path: &str, bins: usize) -> Res<Vec<Vec<f32>>> {
    let p = checked_path(app, path)?;
    let e = clips::entry(&p).map_err(|e| format!("{e:#}"))?;
    let key = format!("{}|{}|{}", e.path, e.modified, bins);
    if let Some(v) = st(app).peaks.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return Ok(v);
    }
    let v = clipback_core::media::audio_peaks(&p, bins, e.duration).map_err(|e| format!("{e:#}"))?;
    if let Ok(mut m) = st(app).peaks.lock() {
        if m.len() > 64 {
            m.clear();
        }
        m.insert(key, v.clone());
    }
    Ok(v)
}

#[tauri::command]
async fn list_mics() -> Res<Vec<clipback_core::audio::Device>> {
    blocking(|| clipback_core::audio::list_mics().map_err(|e| format!("{e:#}"))).await
}

/// Windows запретила приложениям микрофон (тогда в запись идёт тишина).
#[tauri::command]
fn mic_blocked() -> bool {
    winutil::mic_blocked_by_privacy()
}

/// Открыть страницу Windows «Конфиденциальность → Микрофон».
#[tauri::command]
fn open_mic_privacy() -> Res<()> {
    std::process::Command::new("explorer.exe").arg("ms-settings:privacy-microphone").spawn().map(|_| ()).map_err(err)
}

/// Что слышит сама запись — уровни источников после захвата.
#[tauri::command]
fn audio_levels() -> Vec<clipback_core::mixer::SourceLevel> {
    clipback_core::mixer::levels()
}

/// Сторож микрофона: Windows несколько раз видела громкий сигнал (похоже на голос),
/// а в запись пять минут подряд идёт тишина — значит, звук режет что-то по дороге.
/// Фон шумоподавление убирает законно, поэтому порог высокий и ждём долго.
/// Говорим об этом один раз за запуск записи и пишем в журнал.
fn watch_mic(app: AppHandle) {
    std::thread::spawn(move || {
        let mut warned_gen = 0u64;
        let mut heard = 0u32; // сколько раз Windows видела сигнал, пока в запись шли нули
        let mut was_silent = false;
        let mut privacy_warned = false;
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let (recording, mic_on, device) = {
                let i = lock(&app);
                (i.recorder.is_some(), i.settings.mic, i.settings.mic_device.clone())
            };
            if recording && mic_on && !privacy_warned && winutil::mic_blocked_by_privacy() {
                privacy_warned = true;
                log("микрофон: Windows запретила приложениям доступ к микрофону — в запись идёт тишина");
                show_toast(&app, "Windows не даёт микрофон", "Параметры → Конфиденциальность → Микрофон: включите доступ для классических приложений", "error");
            }
            let Some(mic) = clipback_core::mixer::levels().into_iter().find(|l| l.kind == "mic") else {
                heard = 0;
                continue;
            };
            if !recording || !mic_on {
                continue;
            }
            if mic.zero_secs < 20.0 {
                if was_silent {
                    log("микрофон: звук снова идёт в запись");
                    was_silent = false;
                }
                heard = 0;
                continue;
            }
            // перебор устройств нужен только к пятой минуте тишины (три замера до предупреждения),
            // а не каждые 5 с с двадцатой секунды
            if mic.zero_secs < 280.0 {
                continue;
            }
            let peak = clipback_core::audio::list_mics()
                .ok()
                .and_then(|l| l.into_iter().find(|d| device.as_ref().map_or(d.is_default, |id| &d.id == id)))
                .map_or(0.0, |d| d.peak);
            if peak > 0.05 {
                heard += 1;
            }
            let generation = st(&app).generation.load(Ordering::SeqCst);
            if mic.zero_secs >= 300.0 && heard >= 3 && warned_gen != generation {
                warned_gen = generation;
                was_silent = true;
                log(format!("микрофон: {:.0} с в запись идёт тишина, хотя Windows видит сигнал (пик {peak:.3})", mic.zero_secs));
                show_toast(&app, "Микрофон не слышно в записи", "Windows видит звук, а в запись идёт тишина. Откройте «Настройки → Звук» — там подсказка", "error");
                let _ = app.emit("mic-silent", mic.zero_secs);
            }
        }
    });
}

#[tauri::command]
async fn pick_folder(app: AppHandle, window: tauri::Window) -> Option<String> {
    let start = out_dir(&app);
    // окно выбора привязано к окну, из которого его открыли, — поверх оверлея, а не под ним
    let mut d = app.dialog().file().set_directory(start);
    if let Some(w) = app.get_webview_window(window.label()) {
        d = d.set_parent(&w);
    }
    d.blocking_pick_folder().and_then(|p| p.into_path().ok()).map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn hide_overlay_cmd(app: AppHandle) {
    hide_overlay(&app)
}

#[tauri::command]
fn show_main_cmd(app: AppHandle) {
    hide_overlay(&app);
    show_main(&app)
}

#[tauri::command]
fn frontend_log(window: tauri::Window, message: String) {
    // ошибка в цикле отрисовки повторялась бы каждые 120 мс: одинаковые сообщения — раз
    // в минуту, всего не больше 30 в минуту
    static SEEN: Mutex<(Option<Instant>, u32, Vec<(String, Instant)>)> = Mutex::new((None, 0, Vec::new()));
    let line = format!("[{}] {}", window.label(), message.chars().take(2000).collect::<String>());
    {
        let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let (minute, count, recent) = &mut *g;
        if minute.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            *minute = Some(Instant::now());
            *count = 0;
        }
        recent.retain(|(_, t)| t.elapsed() < Duration::from_secs(60));
        if *count >= 30 || recent.iter().any(|(m, _)| *m == line) {
            return;
        }
        *count += 1;
        recent.push((line.clone(), Instant::now()));
    }
    log(line);
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0)
}

// ---------- запуск ----------

/// После сбоя (программу убили, выключился свет) остаются `.part`: полноценную запись
/// собираем заново по её журналу, недописанный повтор не спасти — он уходит в корзину Windows.
fn recover_parts(app: AppHandle) {
    std::thread::spawn(move || {
        // окно уведомлений должно успеть загрузиться
        std::thread::sleep(Duration::from_secs(3));
        let dir = out_dir(&app);
        for part in clips::stale_parts(&dir, Duration::from_secs(120)) {
            if clipback_core::journal::path_for(&part).exists() {
                match clipback_core::journal::recover(&part) {
                    Ok((path, secs)) => {
                        log(format!("запись восстановлена после сбоя ({:.0} с): {}", secs, path.display()));
                        clips_changed(&app);
                        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        show_toast(&app, "Запись восстановлена", &format!("{} · {}", fmt_secs(secs), name), "rec");
                    }
                    Err(e) => log(format!("не удалось восстановить {}: {e:#}", part.display())),
                }
            } else {
                log(format!("недописанный файл в корзину: {}", part.display()));
                if let Err(e) = winutil::move_to_recycle_bin(&part) {
                    log(format!("в корзину не ушло: {e:#}"));
                }
            }
        }
    });
}

/// Остановить запись перед выходом (обычным или ради обновления).
/// Возвращает, шёл ли захват (чтобы включить его снова, если обновление не удалось).
fn stop_for_exit(app: &AppHandle) -> bool {
    let rec = {
        let mut i = lock(app);
        i.start_pending = false;
        i.recorder.take()
    };
    // иначе forward_events примет закрытый канал за падение захвата и запланирует перезапуск
    st(app).generation.fetch_add(1, Ordering::SeqCst);
    let was = rec.is_some();
    drop(rec);
    wait_stopper(app);
    was
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    // копия с правами администратора для настройки драйвера NVIDIA — без окна и проверки второй копии
    if let Some(code) = nvidia::run_task(&args) {
        std::process::exit(code);
    }
    update::finish_previous(&args);
    let start_hidden = args.iter().any(|a| a == "--minimized");

    tauri::Builder::default()
        // второй запуск просто показывает уже открытое окно
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| show_main(app)))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, sc, e| {
                    if e.state() == ShortcutState::Pressed {
                        on_hotkey(app, sc);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        // главное окно открывается там же и того же размера, что в прошлый раз
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_denylist(&["overlay", "toast"])
                .with_state_flags(tauri_plugin_window_state::StateFlags::all() & !tauri_plugin_window_state::StateFlags::VISIBLE)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(update::UpdateState::default())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .setup(move |app| {
            let handle = app.handle().clone();
            let cfg_dir = app.path().app_config_dir()?;
            let log_dir = app.path().app_log_dir().unwrap_or_else(|_| cfg_dir.clone());
            init_log(&log_dir);
            // сообщения ядра (звук, захват) — в тот же журнал
            clipback_core::util::set_logger(|m| log(m));
            log(format!("Clipback {} запущен", env!("CARGO_PKG_VERSION")));

            let settings_path = cfg_dir.join("settings.json");
            let settings = Settings::load(&settings_path);
            let _ = std::fs::create_dir_all(&settings.out_dir);
            // клипы, которые не успели уйти в корзину в прошлый раз
            trash::flush_leftovers(&settings.out_dir);
            let _ = app.asset_protocol_scope().allow_directory(&settings.out_dir, true);
            let record = settings.record_on_start;
            app.manage(AppState {
                inner: Mutex::new(Inner { settings, ..Default::default() }),
                settings_path,
                generation: AtomicU64::new(0),
                toast_seq: AtomicU64::new(0),
                overlay_seq: AtomicU64::new(0),
                peaks: Mutex::new(Default::default()),
                rec_item: Mutex::new(None),
            record_item: Mutex::new(None),
            favorites: Mutex::new(library::Favorites::load(cfg_dir.join("favorites.json"))),
            hidden: Mutex::new(library::Favorites::load(cfg_dir.join("hidden.json"))),
            trash: trash::Trash::default(),
            stopper: Mutex::new(None),
            });

            widgets::init(&handle);
            create_windows(&handle, start_hidden)?;
            sync_autostart(&handle);
            watch_mic(handle.clone());
            power::start(&handle);
            storage::watch(handle.clone());
            recover_parts(handle.clone());
            winutil::install_drag_cursors();
            build_tray(&handle)?;
            register_hotkeys(&handle);
            if record {
                start_recording(&handle);
            } else {
                emit_state(&handle);
            }
            if lock(&handle).settings.check_updates {
                update::check_on_start(handle.clone());
            }
            // автоочистка: через минуту после запуска и дальше раз в полчаса
            let h = handle.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(60));
                auto_clean(&h);
                std::thread::sleep(Duration::from_secs(29 * 60));
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            fix_capture_blocked,
            set_recording,
            save_replay_cmd,
            toggle_record_cmd,
            screenshot,
            list_clips,
            trim_clip,
            rename_clip,
            delete_clip,
            delete_clips,
            undo_delete,
            reveal_clip,
            update::open_releases_page,
            report::save_report,
            report::reveal_report,
            open_clips_folder,
            copy_clip,
            start_drag,
            copy_image,
            set_favorite,
            set_hidden,
            clean_preview,
            save_frame,
            save_edited_image,
            set_settings,
            list_monitors,
            list_mics,
            mic_blocked,
            open_mic_privacy,
            audio_levels,
            list_audio_apps,
            audio_peaks,
            pick_folder,
            hide_overlay_cmd,
            show_main_cmd,
            frontend_log,
            quit_app,
            update::check_update,
            update::install_update,
            update::take_whats_new,
            widgets::mixer_state,
            widgets::set_app_volume,
            widgets::set_app_mute,
            widgets::set_volume_lock,
            widgets::set_master_volume,
            widgets::set_master_mute,
            widgets::set_default_device,
            widgets::set_mic_volume,
            widgets::set_mic_mute,
            widgets::system_load,
        ])
        .build(tauri::generate_context!())
        .expect("не удалось запустить Clipback")
        .run(|app, e| {
            if let tauri::RunEvent::Exit = e {
                // корректно останавливаем захват; то, что ждало «Вернуть», — в корзину
                let _ = stop_for_exit(app);
                st(app).trash.finalize_all();
                log("выход");
            }
        });
}
