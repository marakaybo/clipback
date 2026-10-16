//! Настройки: хранятся в JSON в папке приложения.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Отдельная дорожка приложения (расширенный режим звука).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppTrack {
    pub name: String,
    pub exe: String,
    pub enabled: bool,
}

impl Default for AppTrack {
    fn default() -> Self {
        Self { name: String::new(), exe: String::new(), enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Длина буфера, минуты.
    pub buffer_minutes: f64,
    pub fps: u32,
    /// "low" | "medium" | "high" | "custom"
    pub quality: String,
    /// Битрейт для «custom», Мбит/с.
    pub custom_bitrate: u32,
    /// Уменьшать картинку до этой высоты (None — как на экране).
    pub max_height: Option<u32>,
    pub monitor: usize,
    pub out_dir: PathBuf,
    pub hotkey_overlay: String,
    pub hotkey_save: String,
    pub hotkey_screenshot: String,
    /// Начать / остановить полноценную запись.
    pub hotkey_record: String,
    /// Звук системы (всё, что слышно в наушниках).
    pub system_audio: bool,
    pub mic: bool,
    /// Id микрофона; None — по умолчанию в Windows.
    pub mic_device: Option<String>,
    /// Усиление микрофона (1.0 — как есть).
    pub mic_gain: f32,
    /// Автоусиление голоса (не зависит от громкости микрофона в Windows).
    pub mic_agc: bool,
    /// "simple" — звук системы и микрофон; "advanced" — плюс дорожки приложений.
    pub audio_mode: String,
    pub app_tracks: Vec<AppTrack>,
    /// Писать буфер сразу после запуска программы.
    pub record_on_start: bool,
    /// Крестик прячет окно в трей, а не закрывает программу.
    pub close_to_tray: bool,
    pub autostart: bool,
    /// Проверять новую версию при запуске.
    pub check_updates: bool,
    /// Формат скриншотов: "jpg" (легче) или "png" (без потерь).
    pub screenshot_format: String,
    /// Тихий щелчок при сохранении повтора, скриншота и начале/конце записи.
    pub save_sound: bool,
    /// Полупрозрачный значок в углу экрана, пока идёт запись.
    pub record_indicator: bool,
    /// Курсор мыши в записи.
    pub record_cursor: bool,
    /// Спрашивать перед отправкой клипа в корзину.
    pub confirm_delete: bool,
    /// Автоочистка папки клипов (по умолчанию выключена).
    pub auto_clean: bool,
    /// Держать папку не больше стольких ГБ (0 — без лимита).
    pub clean_max_gb: u32,
    /// Убирать клипы старше стольких дней (0 — не смотреть на возраст).
    pub clean_max_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            buffer_minutes: 5.0,
            fps: 60,
            quality: "medium".into(),
            custom_bitrate: 12,
            max_height: None,
            monitor: 0,
            out_dir: clipback_core::util::videos_dir().join("Clipback"),
            hotkey_overlay: "Alt+Z".into(),
            hotkey_save: "Alt+F10".into(),
            hotkey_screenshot: "Alt+F1".into(),
            hotkey_record: "Alt+F9".into(),
            system_audio: true,
            mic: false,
            mic_device: None,
            mic_gain: 1.0,
            mic_agc: true,
            audio_mode: "simple".into(),
            app_tracks: Vec::new(),
            record_on_start: true,
            close_to_tray: true,
            autostart: true,
            check_updates: true,
            screenshot_format: "jpg".into(),
            save_sound: true,
            record_indicator: true,
            record_cursor: true,
            confirm_delete: true,
            auto_clean: false,
            clean_max_gb: 50,
            clean_max_days: 0,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
            .map(Settings::sanitized)
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        // сначала во временный файл: при сбое не потеряем старые настройки
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Привести значения к допустимым (файл мог быть изменён руками).
    pub fn sanitized(mut self) -> Self {
        let d = Settings::default();
        // «max» из прошлых версий — это теперь «high»
        if self.quality == "max" {
            self.quality = "high".into();
        }
        self.custom_bitrate = self.custom_bitrate.clamp(2, 80);
        self.clean_max_gb = self.clean_max_gb.min(10_000);
        self.clean_max_days = self.clean_max_days.min(3650);
        if !self.mic_gain.is_finite() {
            self.mic_gain = 1.0;
        }
        self.mic_gain = self.mic_gain.clamp(0.25, 8.0);
        if !["simple", "advanced"].contains(&self.audio_mode.as_str()) {
            self.audio_mode = d.audio_mode.clone();
        }
        self.app_tracks.retain(|a| !a.exe.trim().is_empty());
        self.app_tracks.truncate(8);
        if !self.buffer_minutes.is_finite() || self.buffer_minutes <= 0.0 {
            self.buffer_minutes = d.buffer_minutes;
        }
        self.buffer_minutes = self.buffer_minutes.clamp(0.25, 30.0);
        if ![30, 60, 120].contains(&self.fps) {
            self.fps = d.fps;
        }
        if !["low", "medium", "high", "custom"].contains(&self.quality.as_str()) {
            self.quality = d.quality;
        }
        if let Some(h) = self.max_height {
            if !(360..=4320).contains(&h) {
                self.max_height = None;
            }
        }
        if self.out_dir.as_os_str().is_empty() {
            self.out_dir = d.out_dir;
        }
        for (k, def) in [
            (&mut self.hotkey_overlay, d.hotkey_overlay),
            (&mut self.hotkey_save, d.hotkey_save),
            (&mut self.hotkey_screenshot, d.hotkey_screenshot),
            (&mut self.hotkey_record, d.hotkey_record),
        ] {
            if k.trim().is_empty() {
                *k = def;
            }
        }
        self
    }

    /// Битрейт под качество и размер картинки, Мбит/с.
    pub fn bitrate_mbps(&self, height: u32) -> u32 {
        // базовые значения для 1080p60; выше/ниже — пропорционально площади
        if self.quality == "custom" {
            return self.custom_bitrate;
        }
        let base = match self.quality.as_str() {
            "low" => 5.0,
            "high" => 14.0,
            _ => 8.0,
        };
        let h = self.max_height.map_or(height, |m| m.min(height)) as f64;
        let area = (h / 1080.0).powi(2);
        let fps = self.fps as f64 / 60.0;
        (base * area * fps.sqrt()).round().clamp(2.0, 120.0) as u32
    }

    /// Нужно ли перезапускать захват после смены настроек.
    pub fn capture_changed(&self, other: &Settings) -> bool {
        self.fps != other.fps
            || self.quality != other.quality
            || self.max_height != other.max_height
            || self.monitor != other.monitor
            || self.out_dir != other.out_dir
            || self.system_audio != other.system_audio
            || self.mic != other.mic
            || self.mic_device != other.mic_device
            || (self.quality == "custom" && self.custom_bitrate != other.custom_bitrate)
            || self.mic_gain != other.mic_gain
            || self.mic_agc != other.mic_agc
            || self.audio_mode != other.audio_mode
            || self.app_tracks != other.app_tracks
            || self.record_cursor != other.record_cursor
    }

    /// Настройки звука для ядра.
    pub fn audio_config(&self) -> clipback_core::mixer::AudioConfig {
        let apps = if self.audio_mode == "advanced" {
            self.app_tracks
                .iter()
                .filter(|a| a.enabled)
                .map(|a| clipback_core::mixer::AppTrack { name: a.name.clone(), exe: a.exe.clone() })
                .collect()
        } else {
            Vec::new()
        };
        clipback_core::mixer::AudioConfig {
            system: self.system_audio,
            mic: self.mic,
            mic_device: self.mic_device.clone(),
            mic_gain: self.mic_gain,
            mic_agc: self.mic_agc,
            apps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_values_fall_back_to_defaults() {
        let s = Settings {
            buffer_minutes: -3.0,
            fps: 47,
            quality: "ultra".into(),
            max_height: Some(10),
            hotkey_save: " ".into(),
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s.buffer_minutes, 5.0);
        assert_eq!(s.fps, 60);
        assert_eq!(s.quality, "medium");
        assert_eq!(s.max_height, None);
        assert_eq!(s.hotkey_save, "Alt+F10");
    }

    #[test]
    fn buffer_is_clamped() {
        let s = Settings { buffer_minutes: 500.0, ..Settings::default() }.sanitized();
        assert_eq!(s.buffer_minutes, 30.0);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let s: Settings = serde_json::from_str(r#"{"fps": 30}"#).unwrap();
        assert_eq!(s.fps, 30);
        assert_eq!(s.hotkey_overlay, "Alt+Z");
        assert_eq!(s.buffer_minutes, 5.0);
    }

    #[test]
    fn bitrate_scales_with_resolution() {
        let s = Settings::default();
        assert_eq!(s.bitrate_mbps(1080), 8);
        assert!(s.bitrate_mbps(1440) > 12);
        let s720 = Settings { max_height: Some(720), ..Settings::default() };
        assert!(s720.bitrate_mbps(1440) < 5);
        let c = Settings { quality: "custom".into(), custom_bitrate: 20, ..Settings::default() };
        assert_eq!(c.bitrate_mbps(1440), 20);
        assert_eq!(Settings { quality: "max".into(), ..Settings::default() }.sanitized().quality, "high");
    }

    #[test]
    fn roundtrip_through_file() {
        let p = std::env::temp_dir().join("clipback-settings-test.json");
        let s = Settings {
            buffer_minutes: 2.5,
            quality: "high".into(),
            audio_mode: "advanced".into(),
            app_tracks: vec![AppTrack { name: "Discord".into(), exe: "Discord.exe".into(), enabled: true }],
            ..Settings::default()
        };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p), s);
        std::fs::write(&p, "{ not json").unwrap();
        assert_eq!(Settings::load(&p), Settings::default());
    }

    #[test]
    fn capture_changes_detected() {
        let a = Settings::default();
        let b = Settings { buffer_minutes: 10.0, ..a.clone() };
        assert!(!a.capture_changed(&b), "длину буфера меняем без перезапуска");
        let c = Settings { fps: 30, ..a.clone() };
        assert!(a.capture_changed(&c));
        let d = Settings { mic: true, ..a.clone() };
        assert!(a.capture_changed(&d), "микрофон включается перезапуском записи");
    }
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    #[test]
    fn apps_only_in_advanced_mode() {
        let mut s = Settings {
            mic: true,
            app_tracks: vec![
                AppTrack { name: "Discord".into(), exe: "Discord.exe".into(), enabled: true },
                AppTrack { name: "Выключено".into(), exe: "x.exe".into(), enabled: false },
            ],
            ..Settings::default()
        };
        assert!(s.audio_config().apps.is_empty(), "в обычном режиме приложений нет");
        s.audio_mode = "advanced".into();
        let a = s.audio_config();
        assert_eq!(a.apps.len(), 1);
        assert_eq!(a.apps[0].exe, "Discord.exe");
        assert!(a.mic);
    }

    #[test]
    fn junk_audio_values_are_fixed() {
        let s = Settings { mic_gain: f32::NAN, audio_mode: "???".into(), app_tracks: vec![AppTrack::default()], ..Settings::default() }.sanitized();
        assert_eq!(s.mic_gain, 1.0);
        assert_eq!(s.audio_mode, "simple");
        assert!(s.app_tracks.is_empty());
    }
}
