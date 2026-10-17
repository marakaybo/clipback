//! Виджеты оверлея в духе Xbox Game Bar: микшер громкости приложений с «замком»
//! и нагрузка компьютера. Звук и счётчики — в ядре (`volume.rs`, `sysload.rs`),
//! температура видеокарты — NVAPI (`nvidia.rs`).

use crate::settings::VolumeLock;
use crate::{err, lock, log, st, Res};
use clipback_core::{sysload, volume};
use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager};

pub struct Widgets {
    keeper: volume::VolumeKeeper,
    /// Счётчики нагрузки открыты только пока оверлей на экране.
    load: Mutex<Option<sysload::Sampler>>,
}

/// Запустить замок громкости с замками из настроек. Держит громкость, пока Clipback запущен.
pub fn init(app: &AppHandle) {
    let keeper = volume::VolumeKeeper::start();
    keeper.set_locks(lock_list(&lock(app).settings.volume_locks));
    app.manage(Widgets { keeper, load: Mutex::new(None) });
}

fn lock_list(locks: &[VolumeLock]) -> Vec<(String, f32)> {
    locks.iter().map(|l| (l.key.clone(), l.volume)).collect()
}

fn widgets(app: &AppHandle) -> tauri::State<'_, Widgets> {
    app.state::<Widgets>()
}

/// Оверлей спрятан — счётчики больше не нужны.
pub fn overlay_hidden(app: &AppHandle) {
    if let Some(w) = app.try_state::<Widgets>() {
        if let Ok(mut l) = w.load.lock() {
            *l = None;
        }
    }
}

/// Изменить замки, сохранить настройки и передать замки потоку замка.
fn update_locks(app: &AppHandle, f: impl FnOnce(&mut Vec<VolumeLock>)) -> Res<()> {
    let (settings, path) = {
        let mut i = lock(app);
        f(&mut i.settings.volume_locks);
        i.settings = i.settings.clone().sanitized();
        (i.settings.clone(), st(app).settings_path.clone())
    };
    widgets(app).keeper.set_locks(lock_list(&settings.volume_locks));
    settings.save(&path).map_err(|e| format!("не удалось сохранить настройки: {e:#}"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MixerApp {
    #[serde(flatten)]
    app: volume::AppVolume,
    locked: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MixerDto {
    device: String,
    master: f32,
    master_muted: bool,
    apps: Vec<MixerApp>,
}

#[tauri::command]
pub async fn mixer_state(app: AppHandle) -> Res<MixerDto> {
    let m = volume::mixer_state().map_err(|e| format!("{e:#}"))?;
    let locks = lock(&app).settings.volume_locks.clone();
    Ok(MixerDto {
        device: m.device,
        master: m.master,
        master_muted: m.master_muted,
        apps: m
            .apps
            .into_iter()
            .map(|a| MixerApp { locked: locks.iter().any(|l| l.key == a.key), app: a })
            .collect(),
    })
}

#[tauri::command]
pub async fn set_app_volume(app: AppHandle, key: String, volume: f32) -> Res<()> {
    let v = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 };
    // у приложения с замком двигаем и сам замок, иначе он вернёт громкость назад
    if lock(&app).settings.volume_locks.iter().any(|l| l.key == key) {
        update_locks(&app, |l| l.iter_mut().filter(|l| l.key == key).for_each(|l| l.volume = v))?;
    }
    volume::set_app_volume(&key, v).map_err(err)
}

#[tauri::command]
pub async fn set_app_mute(key: String, muted: bool) -> Res<()> {
    volume::set_app_mute(&key, muted).map_err(err)
}

/// Замок: держать текущую громкость приложения (`on`), или отпустить.
#[tauri::command]
pub async fn set_volume_lock(app: AppHandle, key: String, name: String, volume: f32, on: bool) -> Res<()> {
    let key = key.to_lowercase();
    if on {
        log(format!("замок громкости: {key} на {:.0}%", volume * 100.0));
    } else {
        log(format!("замок громкости снят: {key}"));
    }
    update_locks(&app, |l| {
        l.retain(|x| x.key != key);
        if on {
            l.push(VolumeLock { key, name, volume });
        }
    })
}

#[tauri::command]
pub async fn set_master_volume(volume: f32) -> Res<()> {
    volume::set_master_volume(if volume.is_finite() { volume } else { 1.0 }).map_err(err)
}

#[tauri::command]
pub async fn set_master_mute(muted: bool) -> Res<()> {
    volume::set_master_mute(muted).map_err(err)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadDto {
    #[serde(flatten)]
    load: sysload::Load,
    /// °C; None — не NVIDIA или драйвер не ответил.
    gpu_temp: Option<i32>,
}

/// Нагрузка компьютера. Оверлей спрашивает раз в секунду, пока открыт.
#[tauri::command]
pub async fn system_load(app: AppHandle) -> Res<LoadDto> {
    let w = widgets(&app);
    let fresh = w.load.lock().map_err(err)?.is_none();
    if fresh {
        let s = sysload::Sampler::new().map_err(|e| format!("{e:#}"))?;
        *w.load.lock().map_err(err)? = Some(s);
        // проценты считаются между двумя замерами
        let _ = tauri::async_runtime::spawn_blocking(|| std::thread::sleep(Duration::from_millis(250))).await;
    }
    let load = match w.load.lock().map_err(err)?.as_mut() {
        Some(s) => s.sample(),
        None => return Err("оверлей закрыт".into()),
    };
    Ok(LoadDto { load, gpu_temp: crate::nvidia::gpu_temperature() })
}
