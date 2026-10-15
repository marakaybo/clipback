//! Приложение Clipback: главное окно, оверлей по Alt+Z, уведомления, трей и горячие клавиши.
//! Запись идёт в ядре (`clipback-core`), здесь — управление им и интерфейс.

mod clips;
mod indicator;
mod library;
mod settings;
mod sound;
mod update;
mod winutil;

use base64::Engine;
use clipback_core::{gpu, util, Command, Config, Event, Recorder};
use serde::Serialize;
use settings::Settings;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;
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

#[derive(Default)]
struct Inner {
    settings: Settings,
    recorder: Option<Recorder>,
    starting: bool,
    capture: Option<CaptureInfo>,
    status: Option<LiveStatus>,
    error: Option<String>,
    last_game: Option<String>,
    hotkeys: Vec<(Shortcut, Action)>,
    hotkey_errors: Vec<String>,
    /// Полноценная запись идёт с этого момента (мс с 1970 года).
    record_started: Option<u64>,
    /// Запись попросили, когда захват был выключен: начать её сразу после запуска.
    record_after_start: bool,
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
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StateDto {
    recording: bool,
    starting: bool,
    capture: Option<CaptureInfo>,
    status: Option<LiveStatus>,
    error: Option<String>,
    last_game: Option<String>,
    hotkey_errors: Vec<String>,
    record_started: Option<u64>,
    settings: Settings,
    version: &'static str,
    portable: bool,
}

fn dto(i: &Inner) -> StateDto {
    StateDto {
        recording: i.recorder.is_some(),
        starting: i.starting,
        capture: i.capture.clone(),
        status: i.status.clone(),
        error: i.error.clone(),
        last_game: i.last_game.clone(),
        hotkey_errors: i.hotkey_errors.clone(),
        record_started: i.record_started,
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

fn clips_changed(app: &AppHandle) {
    let _ = app.emit("clips-changed", ());
}

// ---------- журнал ----------

static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn log(msg: impl AsRef<str>) {
    let line = format!("[{}] {}\n", util::file_stamp(), msg.as_ref());
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
    if let Some(p) = LOG_PATH.lock().ok().and_then(|g| g.clone()) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

fn init_log(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let p = dir.join("clipback.log");
    // журнал не разрастается: больше 1 МБ — начинаем заново
    if std::fs::metadata(&p).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&p);
    }
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
        if i.recorder.is_some() || i.starting {
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
                        let _ = rec.sender().send(Command::StartRecord { label });
                    }
                    i.recorder = Some(rec);
                }
                emit_state(&app);
                forward_events(app, events, generation);
            }
            Err(e) => {
                log(format!("запись не запустилась: {e:#}"));
                {
                    let mut i = lock(&app);
                    i.starting = false;
                    i.record_after_start = false;
                    i.error = Some(format!("{e:#}"));
                }
                emit_state(&app);
            }
        }
    });
}

fn stop_recording(app: &AppHandle) {
    let rec = {
        let mut i = lock(app);
        i.status = None;
        i.record_started = None;
        i.recorder.take()
    };
    st(app).generation.fetch_add(1, Ordering::SeqCst);
    show_indicator(app, false);
    // остановка ждёт поток захвата (и дописывание файла записи) — делаем это вне блокировки
    drop(rec);
    log("запись остановлена");
    clips_changed(app);
    emit_state(app);
}

fn restart_recording(app: &AppHandle) {
    stop_recording(app);
    start_recording(app);
}

fn forward_events(app: AppHandle, events: mpsc::Receiver<Event>, generation: u64) {
    for ev in events.iter() {
        if st(&app).generation.load(Ordering::SeqCst) != generation {
            break;
        }
        match ev {
            Event::Status { buffered_secs, buffer_bytes, fps, dropped } => {
                lock(&app).status = Some(LiveStatus { buffered_secs, buffer_bytes, fps, dropped });
                emit_state(&app);
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
                show_toast(&app, "Запись сохранена", &format!("{} · {}", fmt_secs(seconds), name), "rec");
            }
            Event::RecordFailed { message } => {
                log(format!("запись не удалась: {message}"));
                lock(&app).record_started = None;
                show_indicator(&app, false);
                emit_state(&app);
                show_toast(&app, "Запись не удалась", &message, "error");
            }
            Event::Error { message } => {
                log(format!("ошибка записи: {message}"));
                let rec = {
                    let mut i = lock(&app);
                    i.error = Some(message.clone());
                    i.status = None;
                    i.record_started = None;
                    i.recorder.take()
                };
                show_indicator(&app, false);
                drop(rec);
                emit_state(&app);
                show_toast(&app, "Запись остановилась", &message, "error");
                break;
            }
            Event::Started { .. } => {}
        }
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
    let mut i = lock(app);
    if let Some(r) = &i.recorder {
        let cmd = if i.record_started.is_some() { Command::StopRecord } else { Command::StartRecord { label } };
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
    let i = lock(app);
    match &i.recorder {
        Some(r) => {
            let _ = r.sender().send(Command::Save { seconds: None, label });
        }
        None => {
            drop(i);
            show_toast(app, "Запись выключена", "Включите запись буфера в Clipback", "error");
        }
    }
}

fn take_screenshot(app: &AppHandle) -> Result<clips::ClipEntry, String> {
    let label = current_game_label(app).unwrap_or_else(|| "Скриншот".into());
    let (tx, rx) = mpsc::channel();
    let (dir, jpeg) = {
        let i = lock(app);
        let r = i.recorder.as_ref().ok_or("запись выключена — скриншот берётся из захвата экрана")?;
        r.sender().send(Command::Screenshot { reply: tx }).map_err(|e| e.to_string())?;
        (i.settings.out_dir.clone(), i.settings.screenshot_format != "png")
    };
    let raw = rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "захват не ответил".to_string())??;
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
    main.on_window_event(move |e| {
        if let WindowEvent::CloseRequested { api, .. } = e {
            if lock(&handle).settings.close_to_tray {
                api.prevent_close();
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
            } else {
                handle.exit(0);
            }
        }
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
    if let Some(item) = st(app).rec_item.lock().ok().and_then(|g| g.clone()) {
        let _ = item.set_checked(d.recording || d.starting);
    }
    if let Some(item) = st(app).record_item.lock().ok().and_then(|g| g.clone()) {
        let _ = item.set_text(if d.record_started.is_some() { "Остановить запись" } else { "Начать запись" });
    }
    if let Some(tray) = app.tray_by_id("main") {
        let tip = match (&d.status, d.recording) {
            _ if d.record_started.is_some() => "Clipback — идёт запись".to_string(),
            (Some(s), true) => format!("Clipback — в буфере {}", fmt_secs(s.buffered_secs)),
            (_, true) => "Clipback — запись идёт".to_string(),
            _ => "Clipback — запись выключена".to_string(),
        };
        let _ = tray.set_tooltip(Some(tip));
    }
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
                    show_toast(&app, "Скриншот сохранён", &e.name, "camera")
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

#[tauri::command]
async fn screenshot(app: AppHandle) -> Res<clips::ClipEntry> {
    take_screenshot(&app)
}

#[tauri::command]
async fn list_clips(app: AppHandle) -> Vec<clips::ClipEntry> {
    let dir = out_dir(&app);
    let mut l = clips::list(&dir);
    favs(&app).mark(&mut l);
    l
}

fn favs(app: &AppHandle) -> std::sync::MutexGuard<'_, library::Favorites> {
    st(app).favorites.lock().unwrap_or_else(|e| e.into_inner())
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

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn trim_clip(
    app: AppHandle,
    path: String,
    start: f64,
    end: f64,
    replace: bool,
    audio_keep: Option<Vec<usize>>,
    quality: Option<String>,
) -> Res<clips::TrimOut> {
    let p = checked_path(&app, &path)?;
    let q = quality.filter(|q| q != "original");
    let progress_app = app.clone();
    let mut last = 0.0;
    let mut progress = move |f: f64| {
        // не чаще чем на 2 % — интерфейсу хватает
        if f - last >= 0.02 || f >= 1.0 {
            last = f;
            let _ = progress_app.emit("export-progress", f);
        }
    };
    let r = clips::trim(&p, start, end, replace, audio_keep.as_deref(), q.as_deref(), &mut progress).map_err(|e| {
        log(format!("не удалось сохранить отрезок: {e:#}"));
        format!("{e:#}")
    })?;
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
    clips_changed(&app);
    Ok(e)
}

#[tauri::command]
async fn delete_clip(app: AppHandle, path: String) -> Res<()> {
    let p = checked_path(&app, &path)?;
    winutil::move_to_recycle_bin(&p).map_err(|e| format!("{e:#}"))?;
    favs(&app).set(&p, false);
    log(format!("в корзину: {}", p.display()));
    clips_changed(&app);
    Ok(())
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
async fn save_frame(app: AppHandle, png_base64: String, label: Option<String>) -> Res<clips::ClipEntry> {
    let data = base64::engine::general_purpose::STANDARD.decode(png_base64.as_bytes()).map_err(err)?;
    let label = label.and_then(|l| clips::clean_name(&l).ok()).unwrap_or_else(|| "Кадр".into());
    let e = clips::save_image(&out_dir(&app), &label, &data).map_err(|e| format!("{e:#}"))?;
    clips_changed(&app);
    Ok(e)
}

#[tauri::command]
async fn save_edited_image(app: AppHandle, path: String, png_base64: String, replace: bool) -> Res<clips::ClipEntry> {
    let p = checked_path(&app, &path)?;
    let data = base64::engine::general_purpose::STANDARD.decode(png_base64.as_bytes()).map_err(err)?;
    let e = clips::save_edited(&p, &data, replace).map_err(|e| format!("{e:#}"))?;
    log(format!("скриншот отредактирован: {}", e.path));
    clips_changed(&app);
    Ok(e)
}

#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) -> Res<Settings> {
    let new = settings.sanitized();
    let (old, path) = {
        let mut i = lock(&app);
        let old = std::mem::replace(&mut i.settings, new.clone());
        (old, st(&app).settings_path.clone())
    };
    new.save(&path).map_err(|e| format!("не удалось сохранить настройки: {e:#}"))?;

    if old.out_dir != new.out_dir {
        let _ = std::fs::create_dir_all(&new.out_dir);
        let _ = app.asset_protocol_scope().allow_directory(&new.out_dir, true);
        clips_changed(&app);
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
    clipback_core::audio::list_audio_apps().map_err(|e| format!("{e:#}"))
}

/// Форма волны звуковых дорожек клипа (считается один раз и запоминается).
#[tauri::command]
async fn audio_peaks(app: AppHandle, path: String, bins: usize) -> Res<Vec<Vec<f32>>> {
    let p = checked_path(&app, &path)?;
    let e = clips::entry(&p).map_err(|e| format!("{e:#}"))?;
    let key = format!("{}|{}|{}", e.path, e.modified, bins);
    if let Some(v) = st(&app).peaks.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return Ok(v);
    }
    let v = clipback_core::media::audio_peaks(&p, bins, e.duration).map_err(|e| format!("{e:#}"))?;
    if let Ok(mut m) = st(&app).peaks.lock() {
        if m.len() > 64 {
            m.clear();
        }
        m.insert(key, v.clone());
    }
    Ok(v)
}

#[tauri::command]
async fn list_mics() -> Res<Vec<clipback_core::audio::Device>> {
    clipback_core::audio::list_mics().map_err(|e| format!("{e:#}"))
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
    log(format!("[{}] {}", window.label(), message.chars().take(2000).collect::<String>()));
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0)
}

// ---------- запуск ----------

/// Остановить запись перед выходом (обычным или ради обновления).
fn stop_for_exit(app: &AppHandle) {
    let rec = lock(app).recorder.take();
    drop(rec);
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
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
            });

            create_windows(&handle, start_hidden)?;
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
            set_recording,
            save_replay_cmd,
            toggle_record_cmd,
            screenshot,
            list_clips,
            trim_clip,
            rename_clip,
            delete_clip,
            reveal_clip,
            open_clips_folder,
            copy_clip,
            start_drag,
            set_favorite,
            clean_preview,
            save_frame,
            save_edited_image,
            set_settings,
            list_monitors,
            list_mics,
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
        ])
        .build(tauri::generate_context!())
        .expect("не удалось запустить Clipback")
        .run(|app, e| {
            if let tauri::RunEvent::Exit = e {
                // корректно останавливаем захват
                stop_for_exit(app);
                log("выход");
            }
        });
}
