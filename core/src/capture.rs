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
    /// `image` — изменилась картинка; если нет, сдвинулась только мышь (текстура — прежний экран).
    New { tex: ID3D11Texture2D, image: bool },
    /// Нового кадра нет: экран не изменился или захват ждёт следующей попытки.
    Timeout,
    /// Захват только что пересоздан — кадров с него ещё не было.
    Lost,
}

/// Когда снова создавать захват после потери. Каждое создание Desktop Duplication заставляет
/// Windows перерисовать курсор, а полноэкранную игру — перестроить вывод. Пересоздавать без
/// пауз нельзя: курсор мигал, а Minecraft на F11 мерцал и подтормаживал (захват пересоздавался
/// десятки и сотни раз в секунду). Поэтому одно правило для любой потери, как в OBS:
/// первая — сразу (обычный Alt+Tab или смена режима экрана), вторая подряд — через 0,5 с,
/// дальше — раз в 3 с. «Подряд» — если с прошлой потери прошло меньше `LOSS_WINDOW`:
/// одного кадра мало, игра на весь экран даёт кадр и тут же снова отбирает захват.
fn retry_delay(losses_in_row: u32) -> Duration {
    match losses_in_row {
        0 | 1 => Duration::ZERO,
        2 => Duration::from_millis(500),
        _ => Duration::from_secs(3),
    }
}

/// Потери чаще, чем раз в столько, считаются идущими подряд.
const LOSS_WINDOW: Duration = Duration::from_secs(10);
/// Пересоздать не вышло (например, открыт экран UAC) — следующая попытка через столько.
const FAIL_RETRY: Duration = Duration::from_millis(250);

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
    /// Потери захвата подряд и время последней — для `retry_delay`.
    losses_in_row: u32,
    last_loss: Option<Instant>,
    /// Пока захвата нет — когда пробовать создать его снова.
    retry_at: Instant,
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
    /// Захват потерян (Windows отобрала его, ошибки не проходят, сторож перезапустил).
    pub losses: u32,
    /// Ошибки при получении кадра.
    pub errors: u32,
    pub last_error: String,
}

/// Текст ошибки одной строкой с кодом (у DXGI он многострочный: «…Включите уровень отладки D3D…»).
fn describe(e: &windows::core::Error) -> String {
    let msg = e.message();
    let first = msg.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    format!("{first} (0x{:08X})", e.code().0 as u32)
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
            losses_in_row: 0,
            last_loss: None,
            retry_at: Instant::now(),
            stats: CapStats::default(),
        };
        d.recreate().context("не удалось начать захват экрана")?;
        d.stats = CapStats::default();
        Ok(d)
    }

    fn recreate(&mut self) -> Result<()> {
        self.drop_dup();
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
                self.retry_at = Instant::now() + FAIL_RETRY;
                Err(e.into())
            }
        }
    }

    fn drop_dup(&mut self) {
        self.release();
        self.dup = None;
    }

    /// Захват потерян (или не оживает): закрываем его и назначаем следующую попытку.
    fn lose(&mut self) {
        let now = Instant::now();
        let in_row = self.last_loss.is_some_and(|t| now.duration_since(t) < LOSS_WINDOW);
        self.losses_in_row = if in_row { self.losses_in_row + 1 } else { 1 };
        self.last_loss = Some(now);
        self.errors_in_row = 0;
        self.stats.losses += 1;
        self.drop_dup();
        self.retry_at = now + retry_delay(self.losses_in_row);
    }

    /// Захват теряется снова и снова (5 раз подряд — несколько Alt+Tab так не выглядят).
    /// Так бывает, когда видеодрайвер выводит игру прямо на монитор, мимо рабочего стола
    /// Windows: эксклюзивный полноэкранный режим, а у OpenGL-игр на NVIDIA (Minecraft) —
    /// и окно без рамки. Desktop Duplication тогда получает кадр только при переподключении,
    /// Windows.Graphics.Capture — тоже ничего (проверено `examples/capture_probe.rs`).
    pub fn keeps_losing(&self) -> bool {
        self.losses_in_row >= 5 && self.last_loss.is_some_and(|t| t.elapsed() < LOSS_WINDOW)
    }

    /// Захват сейчас закрыт и ждёт следующей попытки.
    pub fn waiting(&self) -> bool {
        self.dup.is_none()
    }

    /// Перезапустить захват (экран давно «стоит», а на нём игра во весь экран).
    /// Это такая же потеря, как и остальные: частоту попыток ограничивает `retry_delay`.
    pub fn reset(&mut self) {
        self.lose();
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
        let timeout = Duration::from_millis(timeout_ms.max(1) as u64);
        let Some(dup) = self.dup.clone() else {
            self.check_device()?;
            let left = self.retry_at.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                std::thread::sleep(left.min(timeout));
                return Ok(Frame::Timeout);
            }
            if self.recreate().is_ok() {
                return Ok(Frame::Lost);
            }
            // не выходит дольше 2 с — ищем монитор заново
            let now = Instant::now();
            if self.none_since.is_some_and(|t| now.duration_since(t) > Duration::from_secs(2))
                && self.last_reopen.is_none_or(|t| now.duration_since(t) > Duration::from_secs(2))
            {
                self.last_reopen = Some(now);
                match self.reopen_output() {
                    Ok(()) => return Ok(Frame::Lost),
                    Err(e) => self.stats.last_error = format!("{e:#}"),
                }
            }
            return Ok(Frame::Timeout);
        };
        self.release();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {
                self.holding = true;
                self.errors_in_row = 0;
                self.read_pointer(&dup, &info);
                match resource.and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                    Some(tex) => Ok(Frame::New { tex, image: info.LastPresentTime != 0 }),
                    None => {
                        self.release();
                        Ok(Frame::Timeout)
                    }
                }
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(Frame::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.lose();
                Ok(Frame::Timeout)
            }
            Err(e) => {
                self.errors_in_row += 1;
                self.stats.errors += 1;
                self.stats.last_error = describe(&e);
                self.log_error(&e);
                self.check_device()?;
                if e.code() == DXGI_ERROR_INVALID_CALL && self.errors_in_row < 3 {
                    // кадр мог остаться не отпущенным — отпускаем и пробуем снова, захват не трогаем
                    unsafe {
                        let _ = dup.ReleaseFrame();
                    }
                    std::thread::sleep(Duration::from_millis(5).min(timeout));
                    return Ok(Frame::Timeout);
                }
                // ошибка не проходит — захват потерян
                self.lose();
                Ok(Frame::Timeout)
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
        crate::util::log(format!("захват: {}{more}", describe(e)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_grows_then_stays_at_three_seconds() {
        assert_eq!(retry_delay(1), Duration::ZERO);
        assert_eq!(retry_delay(2), Duration::from_millis(500));
        assert_eq!(retry_delay(3), Duration::from_secs(3));
        assert_eq!(retry_delay(1000), Duration::from_secs(3));
    }
}
