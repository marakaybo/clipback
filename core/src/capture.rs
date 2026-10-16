//! Захват монитора через DXGI Desktop Duplication.
//! В процесс игры ничего не внедряется — античиты на это не реагируют.

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_ERROR_INVALID_CALL, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use crate::cursor::{Pointer, PointerShape};

pub enum Frame {
    /// Новый кадр. Текстуру нужно скопировать и затем вызвать `release`.
    New(ID3D11Texture2D),
    /// За отведённое время экран не изменился.
    Timeout,
    /// Захват потерян (смена режима экрана, UAC, полноэкранная игра) — пересоздан.
    Lost,
}

pub struct Duplicator {
    device: ID3D11Device,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    holding: bool,
    /// Курсор мыши: положение и форма приходят вместе с кадрами.
    pub pointer: Pointer,
    /// Курсор сдвинулся или сменил форму с прошлого раза.
    pub pointer_changed: bool,
    /// Ошибки подряд — пересоздаём захват только если они не проходят сами.
    errors_in_row: u32,
    /// Журнал: одинаковые ошибки пишем не чаще раза в 30 с, со счётчиком.
    err_logged: Option<std::time::Instant>,
    err_unlogged: u32,
}

impl Duplicator {
    pub fn new(device: &ID3D11Device, output: &IDXGIOutput1) -> Result<Self> {
        let mut d = Self {
            device: device.clone(),
            output: output.clone(),
            dup: None,
            holding: false,
            pointer: Pointer::default(),
            pointer_changed: false,
            errors_in_row: 0,
            err_logged: None,
            err_unlogged: 0,
        };
        d.recreate().context("не удалось начать захват экрана")?;
        Ok(d)
    }

    fn recreate(&mut self) -> Result<()> {
        self.dup = None;
        self.holding = false;
        self.dup = Some(unsafe { self.output.DuplicateOutput(&self.device)? });
        Ok(())
    }

    /// Размер текущего захватываемого изображения.
    pub fn size(&self) -> Option<(u32, u32)> {
        let dup = self.dup.as_ref()?;
        let d = unsafe { dup.GetDesc() };
        Some((d.ModeDesc.Width, d.ModeDesc.Height))
    }

    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Frame> {
        if self.dup.is_none() {
            // предыдущее пересоздание не удалось (например, открыт экран UAC) — пробуем снова
            if self.recreate().is_err() {
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(1) as u64));
                return Ok(Frame::Timeout);
            }
            return Ok(Frame::Lost);
        }
        self.release();
        let dup = self.dup.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {
                self.holding = true;
                self.errors_in_row = 0;
                let dup = dup.clone();
                self.read_pointer(&dup, &info);
                // кадр, где сдвинулась только мышь, тоже годится: содержимое то же
                let tex: ID3D11Texture2D = resource.context("пустой кадр")?.cast()?;
                Ok(Frame::New(tex))
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(Frame::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                let _ = self.recreate();
                Ok(Frame::Lost)
            }
            Err(e) => {
                self.errors_in_row += 1;
                self.log_error(&e);
                if e.code() == DXGI_ERROR_INVALID_CALL && self.errors_in_row < 10 {
                    // кадр мог остаться не отпущенным — отпускаем и пробуем снова, захват не трогаем
                    unsafe {
                        let _ = dup.ReleaseFrame();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    return Ok(Frame::Timeout);
                }
                // ошибка не проходит — пересоздаём захват
                self.errors_in_row = 0;
                let _ = self.recreate();
                Ok(Frame::Lost)
            }
        }
    }

    /// Ошибка в журнал — не чаще раза в 30 с, иначе он разрастается на тысячи строк.
    fn log_error(&mut self, e: &windows::core::Error) {
        let now = std::time::Instant::now();
        if self.err_logged.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(30)) {
            self.err_unlogged += 1;
            return;
        }
        let more = if self.err_unlogged > 0 { format!(" (и ещё {} за полминуты)", self.err_unlogged) } else { String::new() };
        crate::util::log(format!("захват: {}{more}", e.message().trim()));
        self.err_logged = Some(now);
        self.err_unlogged = 0;
    }

    fn read_pointer(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            let p = &info.PointerPosition;
            self.pointer.visible = p.Visible.as_bool();
            if self.pointer.visible {
                self.pointer.x = p.Position.x;
                self.pointer.y = p.Position.y;
            }
            self.pointer_changed = true;
        }
        if info.PointerShapeBufferSize > 0 {
            let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut need = 0u32;
            let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let ok = unsafe { dup.GetFramePointerShape(buf.len() as u32, buf.as_mut_ptr() as _, &mut need, &mut si) };
            if ok.is_ok() {
                self.pointer.shape = Some(PointerShape {
                    kind: si.Type,
                    width: si.Width,
                    height: si.Height,
                    pitch: si.Pitch,
                    hot_x: si.HotSpot.x,
                    hot_y: si.HotSpot.y,
                    data: buf,
                });
                self.pointer.shape_version += 1;
                self.pointer_changed = true;
            }
        }
    }

    /// Отпустить кадр — после копирования его текстуры.
    pub fn release(&mut self) {
        if self.holding {
            if let Some(dup) = &self.dup {
                unsafe {
                    let _ = dup.ReleaseFrame();
                }
            }
            self.holding = false;
        }
    }
}

pub fn texture_desc(t: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { t.GetDesc(&mut d) };
    d
}
