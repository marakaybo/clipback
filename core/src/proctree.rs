//! Снимок процессов без открытия процессов и «чей это процесс» для микшера.
//!
//! Звук приложения на WebView2 (WhatsApp, Teams) или CEF (Steam) играет не сам exe, а его
//! вспомогательный процесс: аудиосессия принадлежит `msedgewebview2.exe`. Чтобы в микшере была
//! одна строка «WhatsApp», поднимаемся по родителям от процесса сессии до первого процесса,
//! который не помощник.
//!
//! **Ни один процесс не открывается** (у Battlefield 6 античит в ядре): список процессов,
//! родители и время запуска — из `SystemProcessInformation`, путь к exe —
//! из `SystemProcessIdInformation` (так делает диспетчер задач).

use std::collections::HashMap;
use windows::core::{PCWSTR, PWSTR};
use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};
use windows::Win32::Foundation::UNICODE_STRING;

#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub parent: u32,
    /// Имя exe как в системе («WhatsApp.exe»).
    pub exe: String,
    /// Время запуска (FILETIME) — родитель с тем же pid, но запущенный позже, — чужой процесс.
    pub created: i64,
}

/// Вспомогательные процессы встроенных браузеров: их звук — звук приложения-родителя.
/// Браузеры и Electron-приложения (chrome.exe, Discord.exe) не нужны: у их помощников то же
/// имя exe, что у главного процесса, и строки и так одна.
const HELPERS: &[&str] = &[
    "msedgewebview2.exe",
    "steamwebhelper.exe",
    "cefsharp.browsersubprocess.exe",
    "qtwebengineprocess.exe",
    "epicwebhelper.exe",
    "upc_webhelper.exe",
    "uplaywebcore.exe",
    "jcef_helper.exe",
    "cef_helper.exe",
    "cefsubprocess.exe",
];

pub fn is_helper(exe: &str) -> bool {
    let e = exe.to_ascii_lowercase();
    HELPERS.contains(&e.as_str())
}

/// Чей звук у процесса `pid`: от него вверх по родителям, пока процесс — помощник. Если
/// родителя нет (закрылся) или его pid уже занят другим процессом — остаётся последний
/// найденный помощник. Возвращает pid владельца (сам `pid`, если его нет в снимке).
pub fn owner(procs: &HashMap<u32, Proc>, pid: u32) -> u32 {
    let mut cur = pid;
    for _ in 0..16 {
        let Some(p) = procs.get(&cur) else { break };
        if !is_helper(&p.exe) {
            break;
        }
        let Some(parent) = procs.get(&p.parent).filter(|q| q.pid != p.pid && q.pid != 0 && q.created <= p.created) else { break };
        cur = parent.pid;
    }
    cur
}

/// Все процессы по pid. Ни один процесс при этом не открывается.
pub fn snapshot() -> HashMap<u32, Proc> {
    // начало SYSTEM_PROCESS_INFORMATION (x64)
    #[repr(C)]
    struct Spi {
        next: u32,
        _threads: u32,
        _ws_private: i64,
        _hard_faults: u32,
        _threads_hwm: u32,
        _cycles: u64,
        created: i64,
        _user: i64,
        _kernel: i64,
        name: UNICODE_STRING,
        _priority: i32,
        pid: usize,
        parent: usize,
    }
    let mut out = HashMap::new();
    let mut buf: Vec<u64> = vec![0; 64 * 1024];
    for _ in 0..5 {
        let mut need = 0u32;
        let st = unsafe {
            NtQuerySystemInformation(SYSTEM_INFORMATION_CLASS(5), buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut need)
        };
        if st.is_ok() {
            break;
        }
        // STATUS_INFO_LENGTH_MISMATCH — буфер мал: процессов стало больше
        if st.0 as u32 != 0xC000_0004 {
            return out;
        }
        buf = vec![0; (need as usize / 8 + 16 * 1024).max(buf.len() * 2)];
    }
    let bytes = buf.len() * 8;
    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    while off + std::mem::size_of::<Spi>() <= bytes {
        let e = unsafe { &*(base.add(off) as *const Spi) };
        let exe = if e.name.Buffer.is_null() || e.name.Length == 0 {
            if e.pid == 0 { "Idle".to_string() } else { "System".to_string() }
        } else {
            unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(e.name.Buffer.0, e.name.Length as usize / 2)) }
        };
        out.insert(e.pid as u32, Proc { pid: e.pid as u32, parent: e.parent as u32, exe, created: e.created });
        if e.next == 0 {
            break;
        }
        off += e.next as usize;
    }
    out
}

/// Путь к exe без открытия процесса: `SystemProcessIdInformation`, путь — с буквой диска.
pub fn image_path(pid: u32) -> Option<String> {
    #[repr(C)]
    struct SystemProcessIdInformation {
        pid: usize,
        name: UNICODE_STRING,
    }
    if pid == 0 {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut info = SystemProcessIdInformation {
        pid: pid as usize,
        name: UNICODE_STRING { Length: 0, MaximumLength: (buf.len() * 2) as u16, Buffer: PWSTR(buf.as_mut_ptr()) },
    };
    let st = unsafe {
        NtQuerySystemInformation(
            SYSTEM_INFORMATION_CLASS(88),
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<SystemProcessIdInformation>() as u32,
            std::ptr::null_mut(),
        )
    };
    if st.is_err() || info.name.Length == 0 {
        return None;
    }
    let nt = String::from_utf16_lossy(&buf[..info.name.Length as usize / 2]);
    Some(to_dos_path(&nt, &dos_devices()))
}

/// «\Device\HarddiskVolume3» -> «C:» для всех дисков.
fn dos_devices() -> Vec<(String, String)> {
    use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
    let mut out = Vec::new();
    for l in b'A'..=b'Z' {
        let drive = format!("{}:", l as char);
        let w: Vec<u16> = drive.encode_utf16().chain(Some(0)).collect();
        let mut buf = [0u16; 512];
        let n = unsafe { QueryDosDeviceW(PCWSTR(w.as_ptr()), Some(&mut buf)) } as usize;
        if n > 0 {
            let end = buf[..n].iter().position(|c| *c == 0).unwrap_or(n);
            out.push((String::from_utf16_lossy(&buf[..end]), drive));
        }
    }
    out
}

/// Путь ядра Windows -> обычный путь с буквой диска (если диск известен).
fn to_dos_path(nt: &str, drives: &[(String, String)]) -> String {
    for (dev, drive) in drives {
        if let Some(rest) = nt.strip_prefix(dev.as_str()) {
            if rest.starts_with('\\') {
                return format!("{drive}{rest}");
            }
        }
    }
    nt.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(list: &[(u32, u32, &str, i64)]) -> HashMap<u32, Proc> {
        list.iter().map(|&(pid, parent, exe, created)| (pid, Proc { pid, parent, exe: exe.into(), created })).collect()
    }

    #[test]
    fn webview_audio_belongs_to_its_app() {
        // explorer -> WhatsApp -> msedgewebview2 (браузер) -> msedgewebview2 (звук)
        let t = tree(&[(1, 0, "explorer.exe", 1), (10, 1, "WhatsApp.exe", 2), (11, 10, "msedgewebview2.exe", 3), (12, 11, "msedgewebview2.exe", 4)]);
        assert_eq!(owner(&t, 12), 10);
        assert_eq!(owner(&t, 11), 10);
        assert_eq!(owner(&t, 10), 10);
    }

    #[test]
    fn two_webview_apps_stay_separate() {
        let t = tree(&[
            (1, 0, "explorer.exe", 1),
            (10, 1, "WhatsApp.exe", 2),
            (11, 10, "msedgewebview2.exe", 3),
            (12, 11, "msedgewebview2.exe", 4),
            (20, 1, "ms-teams.exe", 5),
            (21, 20, "msedgewebview2.exe", 6),
            (22, 21, "msedgewebview2.exe", 7),
        ]);
        assert_eq!(owner(&t, 12), 10);
        assert_eq!(owner(&t, 22), 20);
    }

    #[test]
    fn browser_and_electron_unchanged() {
        // звук Chrome и Discord — в их же дочерних процессах с тем же именем exe
        let t = tree(&[(1, 0, "explorer.exe", 1), (30, 1, "chrome.exe", 2), (31, 30, "chrome.exe", 3), (40, 1, "Discord.exe", 4), (41, 40, "Discord.exe", 5)]);
        assert_eq!(owner(&t, 31), 31);
        assert_eq!(owner(&t, 41), 41);
        assert_eq!(t[&owner(&t, 31)].exe, "chrome.exe");
    }

    #[test]
    fn game_is_its_own_owner() {
        // игру запустил лаунчер — звук всё равно игры, не EA app
        let t = tree(&[(1, 0, "explorer.exe", 1), (50, 1, "EADesktop.exe", 2), (51, 50, "bf6.exe", 3), (60, 1, "javaw.exe", 4)]);
        assert_eq!(owner(&t, 51), 51);
        assert_eq!(owner(&t, 60), 60);
    }

    #[test]
    fn orphan_or_reused_parent_keeps_helper() {
        // родитель закрылся
        let t = tree(&[(11, 10, "msedgewebview2.exe", 3), (12, 11, "msedgewebview2.exe", 4)]);
        assert_eq!(owner(&t, 12), 11);
        // pid родителя занял процесс, запущенный позже помощника
        let t = tree(&[(10, 1, "notepad.exe", 9), (11, 10, "msedgewebview2.exe", 3)]);
        assert_eq!(owner(&t, 11), 11);
        // неизвестный pid и петля
        assert_eq!(owner(&t, 99), 99);
        let t = tree(&[(5, 6, "msedgewebview2.exe", 1), (6, 5, "msedgewebview2.exe", 1)]);
        let o = owner(&t, 5);
        assert!(o == 5 || o == 6);
    }

    #[test]
    fn snapshot_sees_this_process_without_opening_it() {
        let s = snapshot();
        let me = std::process::id();
        let p = s.get(&me).expect("свой процесс в снимке");
        assert!(p.exe.to_lowercase().ends_with(".exe"), "{}", p.exe);
        assert!(s.contains_key(&p.parent) || p.parent != 0);
        let path = image_path(me).expect("путь к своему exe");
        assert!(path.len() > 3 && path.as_bytes()[1] == b':', "{path}");
    }
}
