//! Значок «идёт запись» в углу экрана: полупрозрачный белый кружок поверх игры.
//! Это простое окно Windows с попиксельной прозрачностью (а не ещё один WebView) —
//! почти ничего не весит. Клики проходят сквозь него, в запись и скриншоты он не попадает.

use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetSystemMetrics, RegisterClassW, SetWindowDisplayAffinity, ShowWindow, UpdateLayeredWindow, SM_CXSCREEN,
    SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WDA_EXCLUDEFROMCAPTURE, WINDOW_EX_STYLE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

static HWND_VAL: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

/// Картинка значка, BGRA с умноженной альфой (так требует UpdateLayeredWindow).
pub fn render(size: u32) -> Vec<u8> {
    let mut px = vec![0u8; (size * size * 4) as usize];
    let c = size as f32 / 2.0;
    let r_dot = size as f32 * 0.26;
    let r_halo = size as f32 * 0.46;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
            // мягкая тёмная подложка — кружок виден и на светлом фоне
            // (только вокруг точки, под ней — нет, иначе белый становится серым)
            let halo = ((r_halo - d) / (r_halo - r_dot)).clamp(0.0, 1.0).powf(1.6) * 0.22 * (d - r_dot + 0.5).clamp(0.0, 1.0);
            // белая точка со сглаженным краем
            let dot = (r_dot - d + 0.5).clamp(0.0, 1.0) * 0.62;
            let a = dot + halo * (1.0 - dot);
            let white = if a > 0.0 { dot / a } else { 0.0 };
            let v = (white * a * 255.0).round() as u8;
            let i = ((y * size + x) * 4) as usize;
            px[i] = v;
            px[i + 1] = v;
            px[i + 2] = v;
            px[i + 3] = (a * 255.0).round() as u8;
        }
    }
    px
}

fn create() -> Option<HWND> {
    let r = create_inner();
    if let Err(e) = &r {
        crate::log(format!("значок записи: {e}"));
    }
    r.ok()
}

fn create_inner() -> windows::core::Result<HWND> {
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("ClipbackRecIndicator");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let ex: WINDOW_EX_STYLE = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let h = CreateWindowExW(ex, class, w!("Clipback — запись"), WS_POPUP, 0, 0, 1, 1, None, None, Some(inst.into()), None)?;
        let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);

        let size = (30 * GetDpiForSystem().max(96) / 96) as i32;
        let margin = (18 * GetDpiForSystem().max(96) / 96) as i32;
        let pos = POINT { x: GetSystemMetrics(SM_CXSCREEN) - size - margin, y: margin };
        let pixels = render(size as u32);

        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // сверху вниз
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
        let old = SelectObject(mem, bmp.into());
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8, ..Default::default() };
        let shown = UpdateLayeredWindow(
            h,
            Some(screen),
            Some(&pos),
            Some(&SIZE { cx: size, cy: size }),
            Some(mem),
            Some(&POINT::default()),
            windows::Win32::Foundation::COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        shown?;
        Ok(h)
    }
}

/// Показать или спрятать значок. Вызывать из главного потока (там очередь сообщений окон).
pub fn set_visible(on: bool) {
    let mut h = HWND(HWND_VAL.load(Ordering::SeqCst) as _);
    if h.0.is_null() {
        if !on {
            return;
        }
        match create() {
            Some(n) => {
                HWND_VAL.store(n.0 as isize, Ordering::SeqCst);
                h = n;
            }
            None => return,
        }
    }
    unsafe {
        let _ = ShowWindow(h, if on { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
}

/// Окна самого Clipback (оверлей, уведомления) не должны попадать в запись и скриншоты.
pub fn exclude_from_capture(hwnd: isize) {
    unsafe {
        let _ = SetWindowDisplayAffinity(HWND(hwnd as _), WDA_EXCLUDEFROMCAPTURE);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn icon_is_translucent_white_dot() {
        let s = 30;
        let px = super::render(s);
        let at = |x: u32, y: u32| &px[((y * s + x) * 4) as usize..((y * s + x) * 4 + 4) as usize];
        let center = at(15, 15);
        // полупрозрачный белый: альфа около 60 %, цвет = альфа (умноженная альфа, белый)
        assert!((140..=170).contains(&center[3]), "{:?}", center);
        assert_eq!(center[0], center[3]);
        // угол — пустой
        assert_eq!(at(0, 0), &[0, 0, 0, 0]);
        // ни один пиксель не ярче своей альфы (иначе UpdateLayeredWindow рисует мусор)
        assert!(px.chunks_exact(4).all(|p| p[0] <= p[3]));
    }
}
