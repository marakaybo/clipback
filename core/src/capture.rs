//! Захват монитора через DXGI Desktop Duplication.
//! В процесс игры ничего не внедряется — античиты на это не реагируют.

use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};
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
    /// Имя монитора (\\.\DISPLAY1) — чтобы найти его заново, если Windows его «пересоздала».
    name: String,
    /// С какого момента захвата нет (пересоздание не удаётся).
    none_since: Option<Instant>,
    last_reopen: Option<Instant>,
    /// Потерь захвата подряд без единого нового кадра.
    lost_in_row: u32,
    /// Раньше этого момента захват заново не создаём. Каждое пересоздание заставляет
    /// Windows перерисовать курсор, и без паузы он мигает (было ~940 пересозданий в секунду,
    /// пока свёрнутая полноэкранная игра раз за разом отбирала захват).
    retry_at: Option<Instant>,
    /// Счётчики для журнала (сбрасывает тот, кто их пишет).
    pub stats: CapStats,
}

/// Что происходило с захватом — для сводки в журнале.
#[derive(Default, Debug, Clone)]
pub struct CapStats {
    /// Захват пересоздан успешно.
    pub resets: u32,
    /// Пересоздать не вышло.
    pub reset_fails: u32,
    /// Ошибки при получении кадра.
    pub errors: u32,
    pub last_error: String,
}

fn describe(e: &windows::core::Error) -> String {
    format!("{} (0x{:08X})", e.message().trim(), e.code().0 as u32)
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
            name: unsafe { output.GetDesc() }.map(|d| crate::gpu::wide_to_string(&d.DeviceName)).unwrap_or_default(),
            none_since: None,
            last_reopen: None,
            lost_in_row: 0,
            retry_at: None,
            stats: CapStats::default(),
        };
        d.recreate().context("не удалось начать захват экрана")?;
        d.stats = CapStats::default();
        Ok(d)
    }

    fn recreate(&mut self) -> Result<()> {
        self.dup = None;
        self.holding = false;
        match unsafe { self.output.DuplicateOutput(&self.device) } {
            Ok(d) => {
                self.dup = Some(d);
                self.none_since = None;
                self.stats.resets += 1;
                Ok(())
            }
            Err(e) => {
                self.none_since.get_or_insert_with(Instant::now);
                self.stats.reset_fails += 1;
                self.stats.last_error = describe(&e);
                Err(e.into())
            }
        }
    }

    /// Перезапустить захват (экран давно «стоит», а на нём игра во весь экран).
    pub fn reset(&mut self) -> bool {
        self.lost_in_row = 0;
        self.retry_at = None;
        self.recreate().is_ok()
    }

    /// Видеокарта сбросила устройство (драйвер перезапустился, игра «уронила» видеокарту) —
    /// с этим устройством захват уже не оживёт: запись надо запускать заново.
    fn check_device(&self) -> Result<()> {
        if let Err(e) = unsafe { self.device.GetDeviceRemovedReason() } {
            bail!("видеокарта сбросила устройство захвата: {}", describe(&e));
        }
        Ok(())
    }

    /// Найти монитор заново: после смены режима экрана или полноэкранной игры старый
    /// объект монитора может перестать давать захват, а новый — работает.
    fn reopen_output(&mut self) -> Result<()> {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1};
        let luid = unsafe { self.device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()? }.AdapterLuid;
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut a = 0;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
            a += 1;
            let d = unsafe { adapter.GetDesc1()? };
            if (d.AdapterLuid.LowPart, d.AdapterLuid.HighPart) != (luid.LowPart, luid.HighPart) {
                continue;
            }
            let mut o = 0;
            while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
                o += 1;
                let od = unsafe { output.GetDesc()? };
                if crate::gpu::wide_to_string(&od.DeviceName) == self.name {
                    self.output = output.cast()?;
                    return self.recreate();
                }
            }
        }
        bail!("монитор {} больше не подключён к этой видеокарте", self.name)
    }

    /// Размер текущего захватываемого изображения.
    pub fn size(&self) -> Option<(u32, u32)> {
        let dup = self.dup.as_ref()?;
        let d = unsafe { dup.GetDesc() };
        Some((d.ModeDesc.Width, d.ModeDesc.Height))
    }

    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Frame> {
        if self.dup.is_none() {
            self.check_device()?;
            // пауза перед повторной попыткой ещё не прошла
            if let Some(left) = self.retry_at.map(|t| t.saturating_duration_since(Instant::now())) {
                if !left.is_zero() {
                    std::thread::sleep(left.min(Duration::from_millis(timeout_ms.max(1) as u64)));
                    return Ok(Frame::Timeout);
                }
            }
            // предыдущее пересоздание не удалось (например, открыт экран UAC) — пробуем снова
            if self.recreate().is_err() {
                self.retry_at = Some(Instant::now() + Duration::from_millis(200));
                // не выходит дольше 2 с — ищем монитор заново
                let now = Instant::now();
                if self.none_since.is_some_and(|t| now.duration_since(t) > Duration::from_secs(2))
                    && self.last_reopen.is_none_or(|t| now.duration_since(t) > Duration::from_secs(2))
                {
                    self.last_reopen = Some(now);
                    if let Err(e) = self.reopen_output() {
                        self.stats.last_error = format!("{e:#}");
                    }
                }
                if self.dup.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(1) as u64));
                    return Ok(Frame::Timeout);
                }
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
                self.lost_in_row = 0;
                self.retry_at = None;
                let dup = dup.clone();
                self.read_pointer(&dup, &info);
                // кадр, где сдвинулась только мышь, тоже годится: содержимое то же
                let tex: ID3D11Texture2D = resource.context("пустой кадр")?.cast()?;
                Ok(Frame::New(tex))
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(Frame::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.lost_in_row += 1;
                if self.lost_in_row <= 2 {
                    // обычная смена режима экрана — пересоздаём сразу
                    let _ = self.recreate();
                } else {
                    // захват отбирают снова и снова — ждём, с каждым разом дольше (до 2 с)
                    self.dup = None;
                    self.holding = false;
                    let ms = (50u64 << (self.lost_in_row - 3).min(6)).min(2000);
                    self.retry_at = Some(Instant::now() + Duration::from_millis(ms));
                }
                Ok(Frame::Lost)
            }
            Err(e) => {
                self.errors_in_row += 1;
                self.stats.errors += 1;
                self.stats.last_error = describe(&e);
                self.log_error(&e);
                self.check_device()?;
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
