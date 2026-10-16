//! Обновление из самой программы.
//!
//! Новая версия ищется в `latest.json` последнего релиза на GitHub, файл проверяется по подписи
//! (ключ в tauri.conf.json), так что подменить обновление без закрытого ключа нельзя.
//! Установленная версия обновляется установщиком, portable — заменой своего exe рядом с собой.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

#[derive(Default)]
pub struct UpdateState {
    pending: Mutex<Option<Update>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub portable: bool,
}

#[derive(Serialize, Clone)]
struct Progress {
    done: u64,
    total: u64,
}

/// Portable — exe лежит не в папке установки (там рядом всегда есть uninstall.exe).
pub fn is_portable() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| !d.join("uninstall.exe").exists()))
        .unwrap_or(true)
}

fn old_exe(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// После обновления portable: дождаться выхода старой копии и убрать её файл.
pub fn finish_previous(args: &[String]) {
    if install_over(args) {
        std::process::exit(0);
    }
    if let Some(pid) = args.iter().position(|a| a == "--after-update").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<u32>().ok()) {
        wait_process(pid);
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_exe(&exe));
    }
}

fn wait_process(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, 15_000);
            let _ = CloseHandle(h);
        }
    }
}

async fn find(app: &AppHandle) -> Result<Option<Update>, String> {
    let mut b = app.updater_builder();
    if is_portable() {
        b = b.target("windows-x86_64-portable");
    }
    // для проверки обновления на своём сервере
    if let Ok(url) = std::env::var("CLIPBACK_UPDATE_URL") {
        let url = url.parse().map_err(|e| format!("CLIPBACK_UPDATE_URL: {e}"))?;
        b = b.endpoints(vec![url]).map_err(|e| e.to_string())?;
    }
    let updater = b.build().map_err(|e| e.to_string())?;
    updater.check().await.map_err(|e| format!("не удалось проверить обновления: {e}"))
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let found = find(&app).await?;
    let info = found.as_ref().map(|u| UpdateInfo {
        version: u.version.clone(),
        notes: u.body.clone().unwrap_or_default(),
        portable: is_portable(),
    });
    *app.state::<UpdateState>().pending.lock().unwrap_or_else(|e| e.into_inner()) = found;
    Ok(info)
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let pending = app.state::<UpdateState>().pending.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let update = match pending {
        Some(u) => u,
        None => find(&app).await?.ok_or("обновлений нет")?,
    };
    crate::log(format!("обновление до {}: загрузка", update.version));
    let mut done = 0u64;
    let a = app.clone();
    let bytes = update
        .download(
            move |chunk, total| {
                done += chunk as u64;
                let _ = a.emit("update-progress", Progress { done, total: total.unwrap_or(0) });
            },
            || {},
        )
        .await
        .map_err(|e| format!("не удалось скачать обновление: {e}"))?;

    // «что нового» покажем один раз после перезапуска уже в новой версии
    remember_notes(&app, &update.version, update.body.as_deref().unwrap_or(""));
    // запись надо остановить до замены файлов
    crate::stop_for_exit(&app);
    if is_portable() {
        if let Err(e) = replace_self(&bytes) {
            // нет прав на папку (Program Files, защищённая папка, OneDrive) — ставим через
            // временный файл с правами администратора: Windows спросит разрешения
            crate::log(format!("обновление: заменить на месте не вышло ({e}), пробую с правами администратора"));
            replace_elevated(&bytes)?;
            app.exit(0);
            return Ok(());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        crate::log(format!("обновление до {}: перезапуск", update.version));
        std::process::Command::new(exe)
            .args(["--after-update", &std::process::id().to_string()])
            .spawn()
            .map_err(|e| format!("не удалось перезапустить: {e}"))?;
        app.exit(0);
    } else {
        crate::log(format!("обновление до {}: запуск установщика", update.version));
        // установщик закрывает программу, обновляет и запускает её снова
        update.install(bytes).map_err(|e| format!("не удалось запустить установщик: {e}"))?;
    }
    Ok(())
}

/// Запущенный exe нельзя перезаписать, но можно переименовать: старый уходит в .old,
/// новый встаёт на его место. При ошибке всё возвращается как было.
/// Новая версия запускается из временной папки с правами администратора и сама встаёт на место.
fn replace_elevated(bytes: &[u8]) -> Result<(), String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let target = std::env::current_exe().map_err(|e| e.to_string())?;
    let tmp = std::env::temp_dir().join("clipback-update.exe");
    std::fs::write(&tmp, bytes).map_err(|e| format!("не удалось сохранить обновление: {e}"))?;
    let params = format!("--install-over \"{}\" {}", target.display(), std::process::id());
    let r = unsafe {
        ShellExecuteW(None, &HSTRING::from("runas"), &HSTRING::from(tmp.as_os_str()), &HSTRING::from(params), PCWSTR::null(), SW_HIDE)
    };
    if r.0 as isize <= 32 {
        return Err("обновление отменено: Windows не дала прав на папку с программой. Перенесите Clipback, например, в Документы, или скачайте новую версию с GitHub".into());
    }
    Ok(())
}

/// `--install-over <путь> <pid>`: дождаться выхода старой копии, встать на её место
/// и запустить программу снова — уже с обычными правами (через проводник).
fn install_over(args: &[String]) -> bool {
    let Some(i) = args.iter().position(|a| a == "--install-over") else { return false };
    let (Some(target), Some(pid)) = (args.get(i + 1), args.get(i + 2).and_then(|p| p.parse::<u32>().ok())) else { return true };
    wait_process(pid);
    let target = PathBuf::from(target);
    let ok = std::env::current_exe().ok().is_some_and(|me| {
        let old = old_exe(&target);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&target, &old);
        (0..10).any(|_| {
            let r = std::fs::copy(&me, &target).is_ok();
            if !r {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            r
        })
    });
    if !ok {
        // не получилось — возвращаем старую версию на место
        let _ = std::fs::rename(old_exe(&target), &target);
    }
    let _ = std::process::Command::new("explorer.exe").arg(&target).spawn();
    true
}

fn replace_self(bytes: &[u8]) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let new = exe.with_extension("exe.new");
    let old = old_exe(&exe);
    std::fs::write(&new, bytes)?;
    let _ = std::fs::remove_file(&old);
    // антивирус может на мгновение держать файлы — несколько попыток
    let mut r = std::fs::rename(&exe, &old);
    for _ in 0..5 {
        if r.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        r = std::fs::rename(&exe, &old);
    }
    if let Err(e) = r {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(e);
    }
    Ok(())
}

fn notes_file(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("whats-new.json"))
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct WhatsNew {
    pub version: String,
    pub notes: String,
}

fn remember_notes(app: &AppHandle, version: &str, notes: &str) {
    if notes.trim().is_empty() {
        return;
    }
    if let Some(p) = notes_file(app) {
        let w = WhatsNew { version: version.into(), notes: notes.into() };
        let _ = std::fs::write(p, serde_json::to_vec(&w).unwrap_or_default());
    }
}

/// «Что нового» после обновления: отдаётся один раз и только той версии, о которой запомнили.
#[tauri::command]
pub fn take_whats_new(app: AppHandle) -> Option<WhatsNew> {
    let p = notes_file(&app)?;
    let w: WhatsNew = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
    if w.version != env!("CARGO_PKG_VERSION") {
        // запомнили для другой версии (обновление не встало) — подождём
        return None;
    }
    let _ = std::fs::remove_file(&p);
    Some(w)
}

/// Тихая проверка при запуске: если есть новая версия — сказать интерфейсу.
pub fn check_on_start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio_sleep(8).await;
        match check_update(app.clone()).await {
            Ok(Some(info)) => {
                crate::log(format!("доступно обновление {}", info.version));
                let _ = app.emit("update-available", info);
                // для проверки обновления без интерфейса: сразу ставить найденное
                if std::env::var_os("CLIPBACK_UPDATE_AUTO").is_some() {
                    if let Err(e) = install_update(app.clone()).await {
                        crate::log(e);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => crate::log(e),
        }
    });
}

async fn tokio_sleep(secs: u64) {
    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(std::time::Duration::from_secs(secs))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_keeps_names() {
        assert_eq!(old_exe(Path::new(r"C:\a\clipback.exe")), PathBuf::from(r"C:\a\clipback.exe.old"));
    }
}
