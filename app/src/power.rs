//! Сон, пробуждение и выключение Windows.
//!
//! Сообщения о сне (WM_POWERBROADCAST) и выключении (WM_QUERYENDSESSION / WM_ENDSESSION)
//! Windows рассылает всем окнам верхнего уровня. Tauri их наружу не отдаёт, поэтому у нас
//! своё скрытое окно в отдельном потоке: оно не зависит от очереди окон Tauri (та может быть
//! занята WebView) и может спокойно ждать, пока допишется файл записи. Окно «только для
//! сообщений» (HWND_MESSAGE) не подходит — рассылки до него не доходят.
//!
//! Сон: захват останавливаем (запись в файл при этом дописывается и остаётся целой),
//! после пробуждения запускаем заново. Продолжать захват через сон нельзя: звуковые
//! устройства после сна недействительны, Desktop Duplication теряет доступ, а буфер повтора
//! получил бы дыру в несколько часов. Перезапуск идёт обычным путём `start_recording`.
//!
//! Выключение: файл записи дописываем прямо в WM_QUERYENDSESSION — Windows ждёт ответа
//! на него, а после WM_ENDSESSION процесс могут завершить в любой момент. Пока файл
//! дописывается, Windows показывает причину (`ShutdownBlockReasonCreate`).

use crate::{lock, log, register_hotkeys, start_recording, stop_recording};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::AppHandle;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, ENDSESSION_LOGOFF, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Что делать при событиях питания. Отдельно от Windows — чтобы проверять тестами.
#[derive(Default, Debug)]
struct Power {
    /// Компьютер уснул (ждём пробуждения).
    asleep: bool,
    /// Windows завершает работу (ответили на WM_QUERYENDSESSION).
    ending: bool,
    /// Захват шёл до сна/выключения — запустить снова после пробуждения или отмены.
    resume: bool,
    /// Номер сна: поток пробуждения бросает работу, если компьютер успел уснуть снова.
    epoch: u64,
}

impl Power {
    /// Уход в сон. None — повтор того же сообщения; Some(true) — остановить захват.
    fn suspend(&mut self, running: bool) -> Option<bool> {
        if self.asleep {
            return None;
        }
        self.asleep = true;
        self.epoch += 1;
        // если сон пришёл во время отменённого выключения, захват уже остановлен — помним, что он шёл
        self.resume = running || std::mem::take(&mut self.resume);
        Some(running)
    }

    /// Пробуждение (Windows присылает два разных сообщения — реагируем на первое).
    /// Some(true) — запустить захват снова.
    fn wake(&mut self) -> Option<bool> {
        if !self.asleep {
            return None;
        }
        self.asleep = false;
        Some(std::mem::take(&mut self.resume))
    }

    /// Windows спрашивает, можно ли завершать работу. Some(true) — остановить захват.
    fn query_end(&mut self, running: bool) -> Option<bool> {
        if self.ending {
            return None;
        }
        self.ending = true;
        self.resume = running || std::mem::take(&mut self.resume);
        Some(running)
    }

    /// Выключение отменили (другая программа не дала). Some(true) — запустить захват снова.
    fn end_cancelled(&mut self) -> Option<bool> {
        if !self.ending {
            return None;
        }
        self.ending = false;
        if self.asleep {
            // запустим после пробуждения
            return Some(false);
        }
        Some(std::mem::take(&mut self.resume))
    }
}

static POWER: Mutex<Power> = Mutex::new(Power { asleep: false, ending: false, resume: false, epoch: 0 });
static APP: OnceLock<AppHandle> = OnceLock::new();

fn power() -> std::sync::MutexGuard<'static, Power> {
    POWER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Захват идёт (или запускается) и идёт ли запись в файл.
fn capture_state(app: &AppHandle) -> (bool, bool) {
    let i = lock(app);
    (i.recorder.is_some() || i.starting, i.record_started.is_some())
}

/// Запустить слежение (один раз, при старте программы).
pub fn start(app: &AppHandle) {
    if APP.set(app.clone()).is_err() {
        return;
    }
    let r = std::thread::Builder::new().name("clipback-power".into()).spawn(|| {
        if let Err(e) = window_loop() {
            log(format!("сон и выключение: не удалось создать окно ({e})"));
        }
    });
    if let Err(e) = r {
        log(format!("сон и выключение: {e}"));
    }
}

fn window_loop() -> windows::core::Result<()> {
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("ClipbackPower");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        // окно верхнего уровня, но никогда не показывается и не попадает на панель задач
        let _h = CreateWindowExW(WS_EX_TOOLWINDOW, class, w!("Clipback"), WS_POPUP, 0, 0, 0, 0, None, None, Some(inst.into()), None)?;
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(app) = APP.get() else { return DefWindowProcW(h, m, wp, lp) };
    match m {
        WM_POWERBROADCAST => {
            match wp.0 as u32 {
                PBT_APMSUSPEND => on_suspend(app),
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => on_wake(app),
                _ => {}
            }
            LRESULT(1)
        }
        WM_QUERYENDSESSION => {
            on_query_end(app, h, lp.0 as u32 & ENDSESSION_LOGOFF != 0);
            LRESULT(1) // выключению не мешаем
        }
        WM_ENDSESSION => {
            if wp.0 == 0 {
                on_end_cancelled(app);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

fn what_stops(running: bool, live: bool) -> &'static str {
    if live {
        "дописываю файл записи и останавливаю захват"
    } else if running {
        "останавливаю захват"
    } else {
        "захват не шёл"
    }
}

fn on_suspend(app: &AppHandle) {
    let (running, live) = capture_state(app);
    let Some(stop) = power().suspend(running) else { return };
    log(format!("сон: {}", what_stops(running, live)));
    // Windows даёт на это около 2 с; если файл не успеет дописаться — допишется после пробуждения
    if stop {
        stop_recording(app);
    }
}

fn on_wake(app: &AppHandle) {
    let (restart, epoch) = {
        let mut p = power();
        let Some(restart) = p.wake() else { return };
        (restart, p.epoch)
    };
    log(format!("пробуждение: {}", if restart { "запущу захват, когда откроется рабочий стол" } else { "захват был выключен" }));
    let app = app.clone();
    std::thread::spawn(move || {
        // горячие клавиши обычно переживают сон, но регистрация дешёвая — обновляем на всякий случай
        let h = app.clone();
        let _ = app.run_on_main_thread(move || register_hotkeys(&h));
        if !restart {
            return;
        }
        // после сна сначала экран блокировки: захватить его нельзя, а попытки перезапуска ограничены
        let since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let p = power();
            if p.epoch != epoch || p.asleep || p.ending {
                return;
            }
            drop(p);
            if user_desktop_active() || since.elapsed() > Duration::from_secs(600) {
                break;
            }
        }
        start_recording(&app);
    });
}

/// Рабочий стол пользователя открыт (не экран блокировки и не окно входа).
/// Окно входа живёт на другом рабочем столе, и открыть «текущий» тогда нельзя.
fn user_desktop_active() -> bool {
    use windows::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_SWITCHDESKTOP};
    unsafe {
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_SWITCHDESKTOP) {
            Ok(d) => {
                let _ = CloseDesktop(d);
                true
            }
            Err(_) => false,
        }
    }
}

fn on_query_end(app: &AppHandle, h: HWND, logoff: bool) {
    let (running, live) = capture_state(app);
    let Some(stop) = power().query_end(running) else { return };
    let what = if logoff { "выход из системы" } else { "выключение Windows" };
    log(format!("{what}: {}", what_stops(running, live)));
    if !stop {
        return;
    }
    // причина видна на экране «Эти приложения мешают выключению», только пока дописывается файл
    let blocked = live && unsafe { ShutdownBlockReasonCreate(h, w!("Сохраняется запись экрана")) }.is_ok();
    let t = Instant::now();
    stop_recording(app);
    if blocked {
        unsafe {
            let _ = ShutdownBlockReasonDestroy(h);
        }
        log(format!("{what}: файл записи дописан за {:.1} с", t.elapsed().as_secs_f64()));
    }
}

fn on_end_cancelled(app: &AppHandle) {
    let Some(restart) = power().end_cancelled() else { return };
    log(format!("выключение отменено{}", if restart { ": запускаю захват снова" } else { "" }));
    if restart {
        start_recording(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_stops_and_restarts_once() {
        let mut p = Power::default();
        assert_eq!(p.suspend(true), Some(true));
        assert_eq!(p.suspend(false), None, "повтор сообщения о сне");
        assert_eq!(p.wake(), Some(true));
        assert_eq!(p.wake(), None, "второе сообщение о пробуждении");
    }

    #[test]
    fn sleep_without_capture_does_not_restart() {
        let mut p = Power::default();
        assert_eq!(p.suspend(false), Some(false));
        assert_eq!(p.wake(), Some(false));
    }

    #[test]
    fn wake_without_sleep_is_ignored() {
        let mut p = Power::default();
        assert_eq!(p.wake(), None);
    }

    #[test]
    fn each_sleep_has_new_epoch() {
        let mut p = Power::default();
        p.suspend(true);
        let e = p.epoch;
        p.wake();
        p.suspend(true);
        assert_ne!(p.epoch, e);
    }

    #[test]
    fn shutdown_stops_once_and_cancel_restarts() {
        let mut p = Power::default();
        assert_eq!(p.query_end(true), Some(true));
        assert_eq!(p.query_end(false), None, "Windows спрашивает ещё раз");
        assert_eq!(p.end_cancelled(), Some(true));
        assert_eq!(p.end_cancelled(), None);
    }

    #[test]
    fn cancelled_shutdown_then_sleep_restarts_after_wake() {
        let mut p = Power::default();
        p.query_end(true);
        // сон до отмены выключения: захват уже остановлен, но шёл
        assert_eq!(p.suspend(false), Some(false));
        assert_eq!(p.end_cancelled(), Some(false), "спим — не запускаем");
        assert_eq!(p.wake(), Some(true));
    }

    #[test]
    fn shutdown_without_capture() {
        let mut p = Power::default();
        assert_eq!(p.query_end(false), Some(false));
        assert_eq!(p.end_cancelled(), Some(false));
    }
}
