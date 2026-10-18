//! «Отчёт для разработчика»: один zip с журналом, настройками и сведениями о компьютере.
//! Ничего не отправляется по сети — файл человек пересылает сам.

use std::io::Write;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

/// Заменяет `from` на `to` без учёта регистра (имя пользователя в путях бывает в любом регистре).
fn replace_ci(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_string();
    }
    let (lt, lf) = (text.to_lowercase(), from.to_lowercase());
    // lowercase может менять длину (редкие символы) — тогда заменяем как есть
    if lt.len() != text.len() || lf.len() != from.len() {
        return text.replace(from, to);
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(p) = lt[i..].find(&lf) {
        out.push_str(&text[i..i + p]);
        out.push_str(to);
        i += p + from.len();
    }
    out.push_str(&text[i..]);
    out
}

/// Убирает из текста личные пути: профиль и имя пользователя (в том числе `C:\\Users\\..` из JSON).
pub fn redact(text: &str, user: &str, profile: &str) -> String {
    let mut t = text.to_string();
    if !profile.is_empty() {
        t = replace_ci(&t, &profile.replace('\\', "\\\\"), "%USERPROFILE%");
        t = replace_ci(&t, profile, "%USERPROFILE%");
        t = replace_ci(&t, &profile.replace('\\', "/"), "%USERPROFILE%");
    }
    if user.len() >= 3 {
        t = replace_ci(&t, user, "%USERNAME%");
    }
    t
}

fn run_ps(script: &str) -> String {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; {script}")])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn windows_and_gpu() -> String {
    let os = run_ps("$o=Get-CimInstance Win32_OperatingSystem; \"$($o.Caption) $($o.Version) build $($o.BuildNumber)\"");
    let gpu = run_ps("Get-CimInstance Win32_VideoController | ForEach-Object { \"$($_.Name), driver $($_.DriverVersion) ($($_.DriverDate))\" }");
    let os = if os.is_empty() { "не удалось определить" } else { os.as_str() };
    let gpu = if gpu.is_empty() {
        "  не удалось определить".to_string()
    } else {
        gpu.lines().map(|l| format!("  {}", l.trim())).collect::<Vec<_>>().join("\n")
    };
    format!("Windows: {os}\nВидеокарты:\n{gpu}\n")
}

fn monitors() -> String {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS, HDC, HMONITOR, MONITORINFOEXW};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> windows::core::BOOL {
        let out = &mut *(data.0 as *mut Vec<String>);
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(m, &mut mi as *mut _ as *mut _).as_bool() {
            return true.into();
        }
        let r = mi.monitorInfo.rcMonitor;
        let name = String::from_utf16_lossy(&mi.szDevice).trim_end_matches('\0').to_string();
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        let hz = if EnumDisplaySettingsW(PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool() { dm.dmDisplayFrequency } else { 0 };
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let primary = if mi.monitorInfo.dwFlags & 1 != 0 { ", основной" } else { "" };
        out.push(format!("  {}: {}x{}, {} Гц, масштаб {}%{}", name.trim_start_matches(r"\\.\"), r.right - r.left, r.bottom - r.top, hz, dx * 100 / 96, primary));
        true.into()
    }

    let mut list: Vec<String> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    if list.is_empty() {
        list.push("  не удалось определить".into());
    }
    format!("Мониторы:\n{}\n", list.join("\n"))
}

fn audio(mic_id: &Option<String>, mic_on: bool, system_on: bool) -> String {
    let mut s = String::from("Звук:\n");
    s.push_str(&format!("  звук системы: {}\n", if system_on { "включён" } else { "выключен" }));
    if !mic_on {
        s.push_str("  микрофон: выключен\n");
        return s;
    }
    match clipback_core::audio::list_mics() {
        Ok(list) => {
            let used = list.iter().find(|d| match mic_id {
                Some(id) => &d.id == id,
                None => d.is_default,
            });
            match used {
                Some(d) => s.push_str(&format!(
                    "  микрофон: {}{}, громкость {}%{}\n",
                    d.name,
                    if mic_id.is_none() { " (по умолчанию)" } else { "" },
                    (d.volume * 100.0).round(),
                    if d.muted { ", отключён в Windows" } else { "" }
                )),
                None => s.push_str("  микрофон: выбранное устройство не найдено\n"),
            }
            s.push_str(&format!("  всего микрофонов: {}\n", list.len()));
        }
        Err(e) => s.push_str(&format!("  микрофон: не удалось получить список ({e:#})\n")),
    }
    s
}

fn add(zip: &mut zip::ZipWriter<std::fs::File>, name: &str, data: &[u8]) -> anyhow::Result<()> {
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(name, opts)?;
    zip.write_all(data)?;
    Ok(())
}

fn read_redacted(p: &Path, user: &str, profile: &str) -> Option<String> {
    let bytes = std::fs::read(p).ok()?;
    Some(redact(&String::from_utf8_lossy(&bytes), user, profile))
}

pub fn write_report(dest: &Path, log_dir: &Path, settings_path: &Path, info: &str) -> anyhow::Result<()> {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let profile = std::env::var("USERPROFILE").unwrap_or_default();
    let mut zip = zip::ZipWriter::new(std::fs::File::create(dest)?);
    add(&mut zip, "system.txt", redact(info, &user, &profile).as_bytes())?;
    for name in ["clipback.log", "clipback.old.log"] {
        if let Some(t) = read_redacted(&log_dir.join(name), &user, &profile) {
            add(&mut zip, name, t.as_bytes())?;
        }
    }
    if let Some(t) = read_redacted(settings_path, &user, &profile) {
        add(&mut zip, "settings.json", t.as_bytes())?;
    }
    zip.finish()?;
    Ok(())
}

/// Окно «Сохранить как» → zip. Возвращает путь к файлу или None, если человек передумал.
#[tauri::command]
pub async fn save_report(app: AppHandle, window: tauri::Window) -> Result<Option<String>, String> {
    let stamp = clipback_core::util::file_stamp().replace(' ', "_");
    let mut d = app.dialog().file().set_file_name(format!("clipback-report-{stamp}.zip")).add_filter("Архив zip", &["zip"]);
    if let Ok(dir) = app.path().desktop_dir() {
        d = d.set_directory(dir);
    }
    if let Some(w) = app.get_webview_window(window.label()) {
        d = d.set_parent(&w);
    }
    let Some(dest) = d.blocking_save_file().and_then(|p| p.into_path().ok()) else { return Ok(None) };
    let dest: PathBuf = if dest.extension().is_some() { dest } else { dest.with_extension("zip") };

    let (mic_id, mic_on, system_on) = {
        let g = crate::lock(&app);
        (g.settings.mic_device.clone(), g.settings.mic, g.settings.system_audio)
    };
    let info = tauri::async_runtime::spawn_blocking(move || {
        format!("Clipback {}\n{}{}{}", env!("CARGO_PKG_VERSION"), windows_and_gpu(), monitors(), audio(&mic_id, mic_on, system_on))
    })
    .await
    .map_err(|e| e.to_string())?;
    let log_dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    let settings_path = crate::st(&app).settings_path.clone();
    write_report(&dest, &log_dir, &settings_path, &info).map_err(|e| format!("не удалось сохранить отчёт: {e:#}"))?;
    crate::log(format!("отчёт для разработчика сохранён: {}", dest.display()));
    Ok(Some(dest.to_string_lossy().into_owned()))
}

/// Открыть папку с файлом, выделив его (кнопка «Показать в папке» в уведомлении).
#[tauri::command]
pub async fn reveal_report(app: AppHandle, window: tauri::Window, path: String) -> Result<(), String> {
    let p = PathBuf::from(path);
    if p.extension().and_then(|e| e.to_str()) != Some("zip") || !p.is_file() {
        return Err("файл не найден".into());
    }
    crate::leave_overlay_for_explorer(&app, &window);
    crate::winutil::reveal_in_explorer(&p).map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_profile_and_user() {
        let t = r#"{"outDir":"C:\\Users\\Marakabo\\Videos","x":"c:\users\marakabo\a","n":"marakabo"}"#;
        let r = redact(t, "Marakabo", r"C:\Users\Marakabo");
        assert!(!r.to_lowercase().contains("marakabo"), "{r}");
        assert!(r.contains(r"%USERPROFILE%\\Videos"));
    }

    #[test]
    fn short_user_is_left_alone() {
        assert_eq!(redact("ab ab", "ab", ""), "ab ab");
    }

    #[test]
    fn writes_zip_with_system_info() {
        let dir = std::env::temp_dir().join("clipback-report-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("clipback.log"), "ok").unwrap();
        let dest = dir.join("r.zip");
        let info = format!("{}{}{}", windows_and_gpu(), monitors(), audio(&None, false, true));
        assert!(info.contains("Мониторы:") && info.contains("Видеокарты:"));
        write_report(&dest, &dir, &dir.join("none.json"), &info).unwrap();
        let mut z = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        assert!(z.by_name("system.txt").is_ok() && z.by_name("clipback.log").is_ok());
        println!("{info}");
    }
}
