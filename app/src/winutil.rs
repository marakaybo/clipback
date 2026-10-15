//! Мелкие функции Windows: корзина, «показать в папке», файл в буфер обмена,
//! показ окна без перехвата фокуса.

use anyhow::{bail, Result};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND, POINT};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SHFileOperationW, SHOpenFolderAndSelectItems, DROPFILES, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
    FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().collect()
}

/// Переместить файл в корзину (его можно будет восстановить).
/// Свободное место на диске, где лежит папка (байты).
pub fn free_space(dir: &Path) -> Option<u64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    // папки может ещё не быть — смотрим ближайшую существующую
    let mut d = dir;
    while !d.exists() {
        d = d.parent()?;
    }
    let w: Vec<u16> = d.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None).ok()? };
    Some(free)
}

pub fn move_to_recycle_bin(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("файл не найден");
    }
    let mut from = wide(path);
    from.extend_from_slice(&[0, 0]); // список путей заканчивается двумя нулями
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 || op.fAnyOperationsAborted.as_bool() {
        bail!("не удалось переместить в корзину (код {r}) — файл может быть открыт в другой программе");
    }
    Ok(())
}

/// Открыть папку в Проводнике и выделить файл.
pub fn reveal_in_explorer(path: &Path) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut w = wide(path);
        w.push(0);
        let pidl = ILCreateFromPathW(PCWSTR(w.as_ptr()));
        if pidl.is_null() {
            bail!("файл не найден");
        }
        let r = SHOpenFolderAndSelectItems(pidl, None, 0);
        ILFree(Some(pidl));
        r?;
    }
    Ok(())
}

/// Положить файл в буфер обмена — потом Ctrl+V в Discord/Telegram/Проводнике вставит сам файл.
pub fn copy_file_to_clipboard(path: &Path) -> Result<()> {
    let mut list = wide(path);
    list.extend_from_slice(&[0, 0]);
    let header = std::mem::size_of::<DROPFILES>();
    let bytes = header + list.len() * 2;
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            bail!("не удалось выделить память");
        }
        let df = DROPFILES { pFiles: header as u32, pt: POINT::default(), fNC: false.into(), fWide: true.into() };
        std::ptr::write_unaligned(ptr as *mut DROPFILES, df);
        std::ptr::copy_nonoverlapping(list.as_ptr() as *const u8, ptr.add(header), list.len() * 2);
        let _ = GlobalUnlock(mem);

        if OpenClipboard(None).is_err() {
            let _ = GlobalFree(Some(mem));
            bail!("буфер обмена занят другой программой");
        }
        let _ = EmptyClipboard();
        let r = SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(mem.0)));
        let _ = CloseClipboard();
        if r.is_err() {
            let _ = GlobalFree(Some(mem));
            bail!("не удалось положить файл в буфер обмена");
        }
    }
    Ok(())
}

/// Сделать окно «прозрачным для мыши», не забирающим фокус и без кнопки на панели задач.
pub fn make_passive(hwnd: isize) {
    unsafe {
        let h = HWND(hwnd as _);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let add = (WS_EX_NOACTIVATE.0 | WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0) as isize;
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex | add);
    }
}

/// Показать окно поверх всех, не отнимая фокус у игры.
pub fn show_no_activate(hwnd: isize) {
    unsafe {
        let h = HWND(hwnd as _);
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
    }
}

pub fn hide(hwnd: isize) {
    unsafe {
        let _ = ShowWindow(HWND(hwnd as _), SW_HIDE);
    }
}
