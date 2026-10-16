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

/// Запретила ли Windows приложениям микрофон («Параметры → Конфиденциальность → Микрофон»).
/// Тогда запись получает только тишину, без всякой ошибки — сказать об этом можем только мы.
pub fn mic_blocked_by_privacy() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
    let read = |root: HKEY, path: &str| -> Option<String> {
        let p: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let v: Vec<u16> = "Value".encode_utf16().chain([0]).collect();
        let mut buf = [0u16; 32];
        let mut len = (buf.len() * 2) as u32;
        unsafe { RegGetValueW(root, PCWSTR(p.as_ptr()), PCWSTR(v.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len)) }
            .ok()
            .ok()?;
        Some(String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
    };
    // весь микрофон выключен (HKLM / HKCU) или только для классических приложений (NonPackaged)
    [(HKEY_LOCAL_MACHINE, KEY.to_string()), (HKEY_CURRENT_USER, KEY.to_string()), (HKEY_CURRENT_USER, format!(r"{KEY}\NonPackaged"))]
        .iter()
        .any(|(root, path)| read(*root, path).as_deref() == Some("Deny"))
}

/// Курсор из PNG (прямая альфа) с горячей точкой в долях размера.
fn cursor_from_png(png_bytes: &[u8], hot: (f32, f32)) -> Result<isize> {
    use windows::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut r = dec.read_info()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf)?;
    if info.color_type != png::ColorType::Rgba {
        bail!("курсор: нужен RGBA");
    }
    let (w, h) = (info.width as i32, info.height as i32);
    unsafe {
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // сверху вниз
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        for (d, s) in dst.chunks_exact_mut(4).zip(buf.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
        let mask = CreateBitmap(w, h, 1, 1, None);
        let ii = ICONINFO {
            fIcon: false.into(),
            xHotspot: (w as f32 * hot.0) as u32,
            yHotspot: (h as f32 * hot.1) as u32,
            hbmMask: mask,
            hbmColor: color,
        };
        let cur = CreateIconIndirect(&ii);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        Ok(cur?.0 as isize)
    }
}

/// Курсор-«кулак» на время перетаскивания клипа (и он же с запретом — где бросить нельзя).
/// Размер — под масштаб экрана.
pub fn install_drag_cursors() {
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    let dpi = unsafe { GetDpiForSystem() }.max(96);
    let size = 32 * dpi / 96;
    let (ok, no): (&[u8], &[u8]) = if size >= 56 {
        (include_bytes!("../icons/cursors/grab-64.png"), include_bytes!("../icons/cursors/grab-64-no.png"))
    } else if size >= 40 {
        (include_bytes!("../icons/cursors/grab-48.png"), include_bytes!("../icons/cursors/grab-48-no.png"))
    } else {
        (include_bytes!("../icons/cursors/grab-32.png"), include_bytes!("../icons/cursors/grab-32-no.png"))
    };
    // горячая точка — середина кулака (16, 17 из 32)
    let hot = (0.5, 17.0 / 32.0);
    match (cursor_from_png(ok, hot), cursor_from_png(no, hot)) {
        (Ok(a), Ok(b)) => drag::set_feedback_cursors(a, b),
        (Err(e), _) | (_, Err(e)) => crate::log(format!("курсор перетаскивания: {e:#}")),
    }
}

/// Картинка для буфера обмена: 24-битный DIB (снизу вверх, строки выровнены по 4 байта).
/// Его понимают Discord, Telegram, браузеры, Paint и Office.
pub fn dib_from_rgba(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row = ((w * 3 + 3) & !3) as usize;
    let mut out = Vec::with_capacity(40 + row * h as usize);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // положительная высота — строки снизу вверх
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((row * h as usize) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    for y in (0..h as usize).rev() {
        let src = &rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
        let start = out.len();
        for p in src.chunks_exact(4) {
            out.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        out.resize(start + row, 0);
    }
    out
}

/// Положить картинку в буфер обмена (Ctrl+V в чат вставит её как изображение).
pub fn copy_image_to_clipboard(rgba: &[u8], w: u32, h: u32) -> Result<()> {
    let dib = dib_from_rgba(rgba, w, h);
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, dib.len())?;
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            bail!("не удалось выделить память");
        }
        std::ptr::copy_nonoverlapping(dib.as_ptr(), ptr, dib.len());
        let _ = GlobalUnlock(mem);
        // буфер может быть на мгновение занят другой программой — несколько попыток
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        if !opened {
            let _ = GlobalFree(Some(mem));
            bail!("буфер обмена занят другой программой");
        }
        let _ = EmptyClipboard();
        const CF_DIB: u32 = 8;
        let r = SetClipboardData(CF_DIB, Some(HANDLE(mem.0)));
        let _ = CloseClipboard();
        if r.is_err() {
            let _ = GlobalFree(Some(mem));
            bail!("не удалось положить картинку в буфер обмена");
        }
    }
    Ok(())
}

#[cfg(test)]
mod dib_tests {
    #[test]
    fn mic_privacy_reads_registry() {
        // на машине разработки доступ разрешён; главное — чтение не падает
        println!("микрофон запрещён: {}", super::mic_blocked_by_privacy());
    }

    #[test]
    fn drag_cursors_decode() {
        let png = include_bytes!("../icons/cursors/grab-48-no.png");
        assert!(super::cursor_from_png(png, (0.5, 0.5)).unwrap() != 0);
    }

    #[test]
    fn dib_rows_are_padded_and_bottom_up() {
        // 2×2: верхняя строка красная, нижняя синяя
        let rgba = [255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255];
        let d = super::dib_from_rgba(&rgba, 2, 2);
        assert_eq!(d.len(), 40 + 8 * 2); // строка 6 байт → 8 с выравниванием
        // первая строка в файле — нижняя (синяя), порядок BGR
        assert_eq!(&d[40..43], &[255, 0, 0]);
        assert_eq!(&d[48..51], &[0, 0, 255]);
    }
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
