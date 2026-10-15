use std::path::PathBuf;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::{FOLDERID_Videos, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};

static LOGGER: std::sync::OnceLock<Box<dyn Fn(&str) + Send + Sync>> = std::sync::OnceLock::new();

/// Куда ядро пишет сообщения (приложение направляет их в свой журнал). Без этого — в stderr.
pub fn set_logger(f: impl Fn(&str) + Send + Sync + 'static) {
    let _ = LOGGER.set(Box::new(f));
}

pub fn log(msg: impl AsRef<str>) {
    match LOGGER.get() {
        Some(f) => f(msg.as_ref()),
        None => eprintln!("{}", msg.as_ref()),
    }
}

/// Папка «Видео» пользователя (учитывает перенос в OneDrive и т. п.).
pub fn videos_dir() -> PathBuf {
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Videos, KNOWN_FOLDER_FLAG(0), None) {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())).join("Videos")
}

/// Заголовок окна, которое сейчас в фокусе, и id его процесса.
pub fn foreground_window() -> Option<(String, u32)> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(hwnd, &mut buf);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        Some((String::from_utf16_lossy(&buf[..n.max(0) as usize]), pid))
    }
}

/// Сделать из заголовка окна короткое имя для файла:
/// «Minecraft* 1.21.1 - Singleplayer» -> «Minecraft 1.21.1».
pub fn clip_label(title: &str) -> Option<String> {
    let main = title.split(" - ").next().unwrap_or(title);
    let clean: String = main
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '™' | '®' | '©') && !c.is_control())
        .collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean: String = clean.chars().take(40).collect();
    let clean = clean.trim_end_matches(['.', ' ']).to_string();
    let ignored = ["", "Program Manager", "Clipback"];
    if ignored.contains(&clean.as_str()) {
        None
    } else {
        Some(clean)
    }
}

/// `dir/base.ext`, а если такой файл уже есть — `dir/base (2).ext` и т. д.
pub fn unique_path(dir: &std::path::Path, base: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{base}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({n}).{ext}"));
        n += 1;
    }
    p
}

/// Метка времени для имени файла: 2026-10-15 21-48-03
pub fn file_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

#[cfg(test)]
#[path = "tests/util.rs"]
mod tests;
