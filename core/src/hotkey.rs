//! Глобальные горячие клавиши (работают, даже когда в фокусе игра).

use anyhow::{bail, Result};
use std::sync::mpsc;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

/// Разобрать строку вида "Alt+F10", "Ctrl+Shift+S".
pub fn parse(s: &str) -> Result<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut key = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_uppercase()) {
        match part.as_str() {
            "ALT" => mods |= MOD_ALT,
            "CTRL" | "CONTROL" => mods |= MOD_CONTROL,
            "SHIFT" => mods |= MOD_SHIFT,
            "WIN" => mods |= MOD_WIN,
            k if k.len() > 1 && k.starts_with('F') && k[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
                key = Some(0x70 + k[1..].parse::<u32>().unwrap() - 1); // VK_F1 = 0x70
            }
            k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => key = Some(k.as_bytes()[0] as u32),
            other => bail!("непонятная клавиша «{other}» в «{s}»"),
        }
    }
    match key {
        Some(k) => Ok((mods, k)),
        None => bail!("в «{s}» нет основной клавиши"),
    }
}

/// Зарегистрировать горячую клавишу; `on_press` вызывается из отдельного потока.
pub fn spawn(combo: &str, on_press: impl Fn() + Send + 'static) -> Result<()> {
    let (mods, vk) = parse(combo)?;
    let (tx, rx) = mpsc::channel();
    let combo = combo.to_string();
    std::thread::spawn(move || unsafe {
        // клавиша привязана к потоку, поэтому и регистрация, и цикл сообщений — здесь
        if let Err(e) = RegisterHotKey(None, 1, mods | MOD_NOREPEAT, vk) {
            let _ = tx.send(Err(anyhow::anyhow!(
                "клавиша {combo} уже занята другой программой (часто это оверлей NVIDIA): {e}"
            )));
            return;
        }
        let _ = tx.send(Ok(()));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                on_press();
            }
        }
    });
    rx.recv()?
}

#[cfg(test)]
#[path = "tests/hotkey.rs"]
mod tests;
