//! Виджеты оверлея в духе Xbox Game Bar: микшер громкости приложений с «замком»
//! и нагрузка компьютера. Звук и счётчики — в ядре (`volume.rs`, `sysload.rs`),
//! температура видеокарты — NVAPI (`nvidia.rs`).

use crate::settings::VolumeLock;
use crate::{err, lock, log, st, Res};
use clipback_core::{sysload, volume};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager};

pub struct Widgets {
    keeper: volume::VolumeKeeper,
    /// Последний замер нагрузки и история — их пишет фоновый поток (`sample_load`).
    load: Arc<Mutex<LoadState>>,
}

/// Сколько секунд истории нагрузки хранить (график в оверлее).
const HISTORY: usize = 120;

#[derive(Default)]
struct LoadState {
    last: Option<LoadDto>,
    cpu: VecDeque<f32>,
    gpu: VecDeque<f32>,
}

/// Запустить замок громкости с замками из настроек и замер нагрузки.
/// Оба работают в фоне, пока Clipback запущен.
pub fn init(app: &AppHandle) {
    let keeper = volume::VolumeKeeper::start();
    keeper.set_locks(lock_list(&lock(app).settings.volume_locks));
    let load = Arc::new(Mutex::new(LoadState::default()));
    let l = load.clone();
    let _ = std::thread::Builder::new().name("clipback-load".into()).spawn(move || sample_load(l));
    app.manage(Widgets { keeper, load });
}

/// Нагрузка — раз в секунду всё время: при открытии оверлея цифры и график уже готовы.
/// Это несколько счётчиков Windows (те же, что у диспетчера задач) — на игру не влияет.
fn sample_load(state: Arc<Mutex<LoadState>>) {
    let mut sampler = match sysload::Sampler::new() {
        Ok(s) => s,
        Err(e) => {
            log(format!("нагрузка компьютера: {e:#}"));
            return;
        }
    };
    loop {
        // проценты считаются между двумя замерами — первый замер только запоминает отсчёт
        std::thread::sleep(Duration::from_secs(1));
        let load = sampler.sample();
        let dto = LoadDto { load, gpu_temp: crate::nvidia::gpu_temperature(), cpu_history: Vec::new(), gpu_history: Vec::new() };
        let Ok(mut guard) = state.lock() else { return };
        let s = &mut *guard;
        for (h, v) in [(&mut s.cpu, dto.load.cpu), (&mut s.gpu, dto.load.gpu)] {
            if let Some(v) = v {
                h.push_back(v);
                if h.len() > HISTORY {
                    h.pop_front();
                }
            }
        }
        s.last = Some(dto);
    }
}

fn lock_list(locks: &[VolumeLock]) -> Vec<(String, f32)> {
    locks.iter().map(|l| (l.key.clone(), l.volume)).collect()
}

fn widgets(app: &AppHandle) -> tauri::State<'_, Widgets> {
    app.state::<Widgets>()
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
    outputs: Vec<volume::Endpoint>,
    mic: Option<volume::MicState>,
    mics: Vec<volume::Endpoint>,
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
        outputs: m.outputs,
        mic: m.mic,
        mics: m.mics,
    })
}

/// Сделать устройство вывода или микрофон устройством по умолчанию в Windows.
#[tauri::command]
pub async fn set_default_device(id: String) -> Res<()> {
    log(format!("звук: устройство по умолчанию → {id}"));
    volume::set_default_device(&id).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn set_mic_volume(volume: f32) -> Res<()> {
    volume::set_mic_volume(if volume.is_finite() { volume } else { 1.0 }).map_err(err)
}

#[tauri::command]
pub async fn set_mic_mute(muted: bool) -> Res<()> {
    volume::set_mic_mute(muted).map_err(err)
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

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LoadDto {
    #[serde(flatten)]
    load: sysload::Load,
    /// °C; None — не NVIDIA или драйвер не ответил.
    gpu_temp: Option<i32>,
    /// Процессор и видеокарта за последние `HISTORY` секунд, % (для графика).
    cpu_history: Vec<f32>,
    gpu_history: Vec<f32>,
}

/// Нагрузка компьютера: последний фоновый замер и история. Оверлей спрашивает раз в секунду.
#[tauri::command]
pub async fn system_load(app: AppHandle) -> Res<LoadDto> {
    let w = widgets(&app);
    let s = w.load.lock().map_err(err)?;
    let mut dto = s.last.clone().ok_or("нагрузка ещё считается")?;
    dto.cpu_history = s.cpu.iter().copied().collect();
    dto.gpu_history = s.gpu.iter().copied().collect();
    Ok(dto)
}
