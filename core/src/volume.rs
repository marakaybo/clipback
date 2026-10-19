//! Микшер громкости приложений, как в Xbox Game Bar, и «замок» громкости.
//!
//! Громкость приложения в Windows — это громкость его аудиосессий (`ISimpleAudioVolume`)
//! на устройстве вывода. У одного приложения их бывает несколько (Discord, браузер), поэтому
//! приложение здесь — все сессии одного exe; ключ — имя exe маленькими буквами
//! («discord.exe»), у системных звуков — `@system`. Звук помощника WebView2/CEF
//! (`msedgewebview2.exe`) относится к приложению, которое его запустило (`proctree.rs`):
//! у WhatsApp одна строка «WhatsApp», и замок держит все её сессии.
//!
//! Замок: `VolumeKeeper` держит громкость выбранных приложений. Windows сообщает о каждой
//! смене громкости сессии (`IAudioSessionEvents::OnSimpleVolumeChanged`) и о каждой новой
//! сессии (`IAudioSessionNotification`) — тогда громкость сразу возвращается. На случай,
//! если уведомление не пришло (сменилось устройство вывода, сессия пересоздана), раз в
//! 2 секунды всё равно проверяем все сессии.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

/// Метка наших собственных изменений громкости: на них замок не реагирует.
const OUR_CONTEXT: GUID = GUID::from_u128(0x6c1b_7a3e_52d4_4f0e_9a51_c11b_ac0f_0a01);
/// Запасная проверка замков, если Windows не прислала уведомление.
const POLL: Duration = Duration::from_secs(2);
/// Разница громкости, которую замок считает изменением (0,5 %).
const EPS: f32 = 0.005;
pub const SYSTEM_KEY: &str = "@system";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppVolume {
    /// «discord.exe» или `@system`.
    pub key: String,
    pub name: String,
    pub volume: f32,
    pub muted: bool,
    /// Сейчас выводит звук (иначе сессия есть, но молчит).
    pub active: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MixerState {
    pub device: String,
    pub master: f32,
    pub master_muted: bool,
    pub apps: Vec<AppVolume>,
    /// Устройства вывода (наушники, динамики) — какое сейчас по умолчанию.
    pub outputs: Vec<Endpoint>,
    /// Микрофон по умолчанию (None — микрофонов нет) и все микрофоны.
    pub mic: Option<MicState>,
    pub mics: Vec<Endpoint>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicState {
    pub name: String,
    pub volume: f32,
    pub muted: bool,
}

unsafe fn enumerator() -> Result<IMMDeviceEnumerator> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    Ok(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?)
}

/// Включённые устройства вывода или записи и какое из них по умолчанию.
unsafe fn endpoints(flow: EDataFlow) -> Result<Vec<Endpoint>> {
    let en = enumerator()?;
    let def = en.GetDefaultAudioEndpoint(flow, eConsole).ok().and_then(|d| crate::audio::device_id(&d).ok());
    let col = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
    let mut out = Vec::new();
    for i in 0..col.GetCount()? {
        let d = col.Item(i)?;
        let id = crate::audio::device_id(&d)?;
        out.push(Endpoint { name: crate::audio::device_name(&d), is_default: def.as_deref() == Some(id.as_str()), id });
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(out)
}

unsafe fn default_mic() -> Result<IMMDevice> {
    enumerator()?.GetDefaultAudioEndpoint(eCapture, eConsole).context("нет микрофона")
}

/// Сделать устройство (вывода или записи) устройством по умолчанию — для всех ролей, как
/// переключатель в Xbox Game Bar. У Windows для этого нет открытого API: используется
/// `IPolicyConfig` (тот же, что у EarTrumpet и SoundSwitch), метод `SetDefaultEndpoint` —
/// 11-й после трёх методов IUnknown.
pub fn set_default_device(id: &str) -> Result<()> {
    use std::ffi::c_void;
    use windows::core::{IUnknown, HRESULT, HSTRING};
    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870a_f99c_171d_4f9e_af0d_e63d_f40c_2bc9);
    const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf867_9f50_850a_41cf_9c72_430f_2902_90c8);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL).context("переключение устройств недоступно")?;
        let mut p: *mut c_void = std::ptr::null_mut();
        unk.query(&IID_POLICY_CONFIG, &mut p).ok().context("переключение устройств недоступно")?;
        let pc = IUnknown::from_raw(p); // освободится при выходе
        let vtbl = *(pc.as_raw() as *const *const usize);
        let set: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT = std::mem::transmute(*vtbl.add(13));
        let wid = HSTRING::from(id);
        for role in [eConsole, eMultimedia, eCommunications] {
            set(pc.as_raw(), PCWSTR(wid.as_ptr()), role.0).ok().context("Windows не переключила устройство")?;
        }
        Ok(())
    }
}

pub fn set_mic_volume(volume: f32) -> Result<()> {
    unsafe {
        let ep: IAudioEndpointVolume = default_mic()?.Activate(CLSCTX_ALL, None)?;
        ep.SetMasterVolumeLevelScalar(volume.clamp(0.0, 1.0), &OUR_CONTEXT)?;
        Ok(())
    }
}

pub fn set_mic_mute(muted: bool) -> Result<()> {
    unsafe {
        let ep: IAudioEndpointVolume = default_mic()?.Activate(CLSCTX_ALL, None)?;
        ep.SetMute(muted, &OUR_CONTEXT)?;
        Ok(())
    }
}

struct Session {
    key: String,
    /// Процесс-владелец звука (у WebView2 — само приложение, а не его помощник).
    owner: u32,
    ctrl: IAudioSessionControl2,
    vol: ISimpleAudioVolume,
    active: bool,
}

unsafe fn default_output() -> Result<IMMDevice> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    en.GetDefaultAudioEndpoint(eRender, eConsole).context("нет устройства вывода звука")
}

/// Чьё приложение у процесса сессии.
#[derive(Clone)]
struct Owner {
    pid: u32,
    /// имя exe владельца маленькими буквами — ключ приложения
    key: String,
}

/// pid процесса сессии -> владелец. Снимок процессов берётся, только когда появилась сессия
/// с новым pid; pid закрытых сессий забываются.
static OWNERS: Mutex<Option<HashMap<u32, Owner>>> = Mutex::new(None);
/// ключ приложения -> путь к exe (для значка).
static PATHS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn owners(pids: &[u32]) -> HashMap<u32, Owner> {
    let Ok(mut guard) = OWNERS.lock() else { return HashMap::new() };
    let cache = guard.get_or_insert_with(HashMap::new);
    cache.retain(|pid, _| pids.contains(pid));
    let missing: Vec<u32> = pids.iter().copied().filter(|p| !cache.contains_key(p)).collect();
    if !missing.is_empty() {
        let procs = crate::proctree::snapshot();
        for pid in missing {
            let o = crate::proctree::owner(&procs, pid);
            let Some(p) = procs.get(&o) else { continue };
            let path = crate::proctree::image_path(o);
            let key = p.exe.to_lowercase();
            if let (Some(path), Ok(mut m)) = (&path, PATHS.lock()) {
                m.get_or_insert_with(HashMap::new).insert(key.clone(), path.clone());
            }
            cache.insert(pid, Owner { pid: o, key });
        }
    }
    cache.clone()
}

/// Живые сессии устройства (кроме закрытых), у каждой — ключ приложения-владельца.
unsafe fn sessions(mgr: &IAudioSessionManager2) -> Result<Vec<Session>> {
    let list = mgr.GetSessionEnumerator()?;
    let mut raw = Vec::new();
    for i in 0..list.GetCount()? {
        let Ok(ctrl) = list.GetSession(i) else { continue };
        let state = ctrl.GetState().unwrap_or(AudioSessionStateExpired);
        if state == AudioSessionStateExpired {
            continue;
        }
        let Ok(c2) = ctrl.cast::<IAudioSessionControl2>() else { continue };
        let Ok(vol) = ctrl.cast::<ISimpleAudioVolume>() else { continue };
        let system = c2.IsSystemSoundsSession() == windows::Win32::Foundation::S_OK;
        let pid = if system { 0 } else { c2.GetProcessId().unwrap_or(0) };
        if !system && pid == 0 {
            continue;
        }
        raw.push((c2, vol, pid, state == AudioSessionStateActive));
    }
    let pids: Vec<u32> = raw.iter().map(|r| r.2).filter(|p| *p != 0).collect();
    let owners = owners(&pids);
    let mut out = Vec::new();
    for (ctrl, vol, pid, active) in raw {
        let (key, owner) = if pid == 0 {
            (SYSTEM_KEY.to_string(), 0)
        } else {
            let Some(o) = owners.get(&pid) else { continue };
            (o.key.clone(), o.pid)
        };
        out.push(Session { key, owner, ctrl, vol, active });
    }
    Ok(out)
}

/// Сессии устройства по умолчанию: (pid процесса сессии, pid владельца, ключ приложения).
/// Для проверки и примера `mixer_owners`.
pub fn session_owners() -> Result<Vec<(u32, u32, String)>> {
    unsafe {
        let mut out = Vec::new();
        for s in sessions(&manager(&default_output()?)?)? {
            out.push((s.ctrl.GetProcessId().unwrap_or(0), s.owner, s.key));
        }
        Ok(out)
    }
}

/// PNG-значок приложения из строки микшера (по пути к его exe; процесс не открывается).
pub fn app_icon(key: &str) -> Option<Vec<u8>> {
    let path = PATHS.lock().ok()?.as_ref()?.get(&key.to_lowercase())?.clone();
    crate::appicon::icon_png(&path)
}

unsafe fn manager(dev: &IMMDevice) -> Result<IAudioSessionManager2> {
    Ok(dev.Activate(CLSCTX_ALL, None)?)
}

/// Понятное имя программы: имя приложения Store («WhatsApp») или «Описание файла» из exe
/// («Discord», «Google Chrome»),
/// иначе имя exe с большой буквы. Запоминается.
fn friendly_name(path: Option<String>, key: &str) -> String {
    static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
    if key == SYSTEM_KEY {
        return "Системные звуки".into();
    }
    let cache_key = path.clone().unwrap_or_else(|| key.to_string());
    if let Some(n) = CACHE.lock().ok().and_then(|c| c.as_ref().and_then(|m| m.get(&cache_key).cloned())) {
        return n;
    }
    let name = path
        .as_deref()
        .and_then(|p| crate::appicon::package_name(p).or_else(|| file_description(p)))
        .filter(|d| !d.trim().is_empty() && d.chars().count() <= 40)
        .unwrap_or_else(|| {
            let exe = path.as_deref().and_then(|p| p.rsplit('\\').next()).unwrap_or(key);
            let stem = exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(exe);
            stem.chars().take(1).flat_map(|c| c.to_uppercase()).chain(stem.chars().skip(1)).collect()
        });
    if let Ok(mut c) = CACHE.lock() {
        c.get_or_insert_with(HashMap::new).insert(cache_key, name.clone());
    }
    name
}

/// FileDescription из ресурсов версии exe.
fn file_description(path: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr() as *mut _).ok()?;
        let query = |sub: &str| -> Option<(*const u8, u32)> {
            let w: Vec<u16> = sub.encode_utf16().chain(Some(0)).collect();
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            VerQueryValueW(data.as_ptr() as *const _, PCWSTR(w.as_ptr()), &mut p, &mut len).as_bool().then_some((p as *const u8, len))
        };
        // первая пара «язык + кодировка»; если её нет — английский Unicode
        let (lang, cp) = query("\\VarFileInfo\\Translation")
            .filter(|(_, len)| *len >= 4)
            .map(|(p, _)| {
                let t = std::slice::from_raw_parts(p as *const u16, 2);
                (t[0], t[1])
            })
            .unwrap_or((0x0409, 0x04B0));
        let (p, len) = query(&format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\FileDescription"))?;
        let s = std::slice::from_raw_parts(p as *const u16, len as usize);
        let s = String::from_utf16_lossy(s);
        Some(s.trim_end_matches('\0').trim().to_string())
    }
}

/// Что сейчас играет и громкость каждого приложения. Сам Clipback не показываем.
pub fn mixer_state() -> Result<MixerState> {
    unsafe {
        let dev = default_output()?;
        let ep: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None)?;
        let me = std::process::id();
        let mut apps: Vec<AppVolume> = Vec::new();
        for s in sessions(&manager(&dev)?)? {
            // свой звук (и звук своего WebView2) не показываем
            if s.owner == me {
                continue;
            }
            let volume = s.vol.GetMasterVolume().unwrap_or(1.0);
            let muted = s.vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
            if let Some(a) = apps.iter_mut().find(|a| a.key == s.key) {
                // у приложения несколько сессий: показываем громкость звучащей
                if s.active && !a.active {
                    a.volume = volume;
                    a.muted = muted;
                }
                a.active |= s.active;
                continue;
            }
            let path = PATHS.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.get(&s.key).cloned()));
            let name = friendly_name(path, &s.key);
            apps.push(AppVolume { key: s.key, name, volume, muted, active: s.active });
        }
        sort_apps(&mut apps);
        let mic = default_mic().ok().and_then(|d| {
            let v: IAudioEndpointVolume = d.Activate(CLSCTX_ALL, None).ok()?;
            Some(MicState {
                name: crate::audio::device_name(&d),
                volume: v.GetMasterVolumeLevelScalar().unwrap_or(1.0),
                muted: v.GetMute().map(|b| b.as_bool()).unwrap_or(false),
            })
        });
        Ok(MixerState {
            device: crate::audio::device_name(&dev),
            master: ep.GetMasterVolumeLevelScalar().unwrap_or(1.0),
            master_muted: ep.GetMute().map(|b| b.as_bool()).unwrap_or(false),
            apps,
            outputs: endpoints(eRender).unwrap_or_default(),
            mic,
            mics: endpoints(eCapture).unwrap_or_default(),
        })
    }
}

/// По алфавиту, системные звуки — в конце: строки не прыгают, пока их двигают.
fn sort_apps(apps: &mut [AppVolume]) {
    apps.sort_by_key(|a| (a.key == SYSTEM_KEY, a.name.to_lowercase()));
}

/// Применить к каждой сессии приложения `key`. Возвращает, сколько сессий нашлось.
fn for_app(key: &str, f: impl Fn(&ISimpleAudioVolume) -> windows::core::Result<()>) -> Result<usize> {
    unsafe {
        let dev = default_output()?;
        let mut n = 0;
        for s in sessions(&manager(&dev)?)? {
            if s.key.eq_ignore_ascii_case(key) {
                f(&s.vol)?;
                n += 1;
            }
        }
        Ok(n)
    }
}

pub fn set_app_volume(key: &str, volume: f32) -> Result<()> {
    let v = volume.clamp(0.0, 1.0);
    for_app(key, |s| unsafe { s.SetMasterVolume(v, &OUR_CONTEXT) })?;
    Ok(())
}

pub fn set_app_mute(key: &str, muted: bool) -> Result<()> {
    for_app(key, |s| unsafe { s.SetMute(muted, &OUR_CONTEXT) })?;
    Ok(())
}

pub fn set_master_volume(volume: f32) -> Result<()> {
    unsafe {
        let ep: IAudioEndpointVolume = default_output()?.Activate(CLSCTX_ALL, None)?;
        ep.SetMasterVolumeLevelScalar(volume.clamp(0.0, 1.0), &OUR_CONTEXT)?;
        Ok(())
    }
}

pub fn set_master_mute(muted: bool) -> Result<()> {
    unsafe {
        let ep: IAudioEndpointVolume = default_output()?.Activate(CLSCTX_ALL, None)?;
        ep.SetMute(muted, &OUR_CONTEXT)?;
        Ok(())
    }
}

// ---------- замок ----------

struct Shared {
    /// ключ приложения -> громкость, которую держим
    locks: Mutex<HashMap<String, f32>>,
    /// сколько раз громкость возвращали (для журнала и проверок)
    restored: AtomicU64,
    /// сколько раз будило уведомление Windows (а не запасная проверка)
    notified: AtomicU64,
}

impl Shared {
    fn target(&self, key: &str) -> Option<f32> {
        self.locks.lock().ok()?.get(key).copied()
    }
}

/// Держит громкость приложений с замком, пока жив. Работает в своём потоке.
pub struct VolumeKeeper {
    shared: Arc<Shared>,
    wake: Sender<Wake>,
}

enum Wake {
    Check,
    Stop,
}

impl VolumeKeeper {
    pub fn start() -> Self {
        let shared = Arc::new(Shared { locks: Mutex::new(HashMap::new()), restored: AtomicU64::new(0), notified: AtomicU64::new(0) });
        let (tx, rx) = std::sync::mpsc::channel();
        let (s, t) = (shared.clone(), tx.clone());
        std::thread::Builder::new()
            .name("volume-keeper".into())
            .spawn(move || keeper_thread(s, t, rx))
            .expect("поток замка громкости");
        Self { shared, wake: tx }
    }

    /// Заменить список замков: (ключ приложения, громкость 0..1).
    pub fn set_locks(&self, locks: impl IntoIterator<Item = (String, f32)>) {
        if let Ok(mut m) = self.shared.locks.lock() {
            *m = locks.into_iter().map(|(k, v)| (k.to_lowercase(), v.clamp(0.0, 1.0))).collect();
        }
        let _ = self.wake.send(Wake::Check);
    }

    pub fn restored(&self) -> u64 {
        self.shared.restored.load(Ordering::Relaxed)
    }

    pub fn notified(&self) -> u64 {
        self.shared.notified.load(Ordering::Relaxed)
    }
}

impl Drop for VolumeKeeper {
    fn drop(&mut self) {
        let _ = self.wake.send(Wake::Stop);
    }
}

/// Слушатель одной сессии: чужая смена громкости приложения с замком будит поток замка.
#[windows::core::implement(IAudioSessionEvents)]
struct SessionSink {
    key: String,
    shared: Arc<Shared>,
    wake: Sender<Wake>,
}

impl IAudioSessionEvents_Impl for SessionSink_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, volume: f32, _muted: windows::core::BOOL, ctx: *const GUID) -> windows::core::Result<()> {
        let ours = !ctx.is_null() && unsafe { *ctx } == OUR_CONTEXT;
        if !ours && self.shared.target(&self.key).is_some_and(|t| (t - volume).abs() > EPS) {
            self.shared.notified.fetch_add(1, Ordering::Relaxed);
            let _ = self.wake.send(Wake::Check);
        }
        Ok(())
    }
    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _: AudioSessionState) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> windows::core::Result<()> {
        let _ = self.wake.send(Wake::Check);
        Ok(())
    }
}

/// Новая сессия на устройстве (приложение запустили или оно пересоздало звук).
#[windows::core::implement(IAudioSessionNotification)]
struct NewSessionSink(Sender<Wake>);

impl IAudioSessionNotification_Impl for NewSessionSink_Impl {
    fn OnSessionCreated(&self, _: windows::core::Ref<'_, IAudioSessionControl>) -> windows::core::Result<()> {
        let _ = self.0.send(Wake::Check);
        Ok(())
    }
}

/// Подписки на одном устройстве вывода.
struct Watch {
    device_id: String,
    mgr: IAudioSessionManager2,
    new_sessions: IAudioSessionNotification,
    /// id экземпляра сессии -> (сессия, слушатель)
    sessions: HashMap<String, (IAudioSessionControl2, IAudioSessionEvents)>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe {
            for (c, sink) in self.sessions.values() {
                let _ = c.UnregisterAudioSessionNotification(sink);
            }
            let _ = self.mgr.UnregisterSessionNotification(&self.new_sessions);
        }
    }
}

fn keeper_thread(shared: Arc<Shared>, wake: Sender<Wake>, rx: Receiver<Wake>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut watch: Option<Watch> = None;
    let mut logged: HashMap<String, u64> = HashMap::new();
    loop {
        let idle = shared.locks.lock().map(|m| m.is_empty()).unwrap_or(true);
        // без замков подписки не нужны — поток просто ждёт
        let msg = if idle {
            watch = None;
            rx.recv().unwrap_or(Wake::Stop)
        } else {
            match rx.recv_timeout(POLL) {
                Ok(m) => m,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Wake::Check,
                Err(_) => Wake::Stop,
            }
        };
        if matches!(msg, Wake::Stop) {
            return;
        }
        // несколько уведомлений подряд — одна проверка
        while let Ok(m) = rx.try_recv() {
            if matches!(m, Wake::Stop) {
                return;
            }
        }
        if shared.locks.lock().map(|m| m.is_empty()).unwrap_or(true) {
            continue;
        }
        if let Err(e) = unsafe { enforce(&shared, &wake, &mut watch, &mut logged) } {
            watch = None;
            crate::util::log(format!("замок громкости: {e:#}"));
            std::thread::sleep(POLL);
        }
    }
}

/// Проверить все сессии и вернуть громкость приложениям с замком.
unsafe fn enforce(shared: &Arc<Shared>, wake: &Sender<Wake>, watch: &mut Option<Watch>, logged: &mut HashMap<String, u64>) -> Result<()> {
    let dev = default_output()?;
    let id = {
        let p = dev.GetId()?;
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    };
    // сменилось устройство вывода — подписываемся заново
    if watch.as_ref().is_none_or(|w| w.device_id != id) {
        *watch = None;
        let mgr = manager(&dev)?;
        let new_sessions: IAudioSessionNotification = NewSessionSink(wake.clone()).into();
        mgr.RegisterSessionNotification(&new_sessions)?;
        *watch = Some(Watch { device_id: id, mgr, new_sessions, sessions: HashMap::new() });
    }
    let w = watch.as_mut().expect("подписка создана выше");
    // перечисление заодно включает уведомления о новых сессиях (так устроен Windows)
    let list = sessions(&w.mgr)?;
    let mut alive = Vec::new();
    for s in &list {
        let Some(target) = shared.target(&s.key) else { continue };
        let Ok(inst) = s.ctrl.GetSessionInstanceIdentifier() else { continue };
        let inst_id = inst.to_string().unwrap_or_default();
        CoTaskMemFree(Some(inst.0 as *const _));
        if !w.sessions.contains_key(&inst_id) {
            let sink: IAudioSessionEvents = SessionSink { key: s.key.clone(), shared: shared.clone(), wake: wake.clone() }.into();
            if s.ctrl.RegisterAudioSessionNotification(&sink).is_ok() {
                w.sessions.insert(inst_id.clone(), (s.ctrl.clone(), sink));
            }
        }
        alive.push(inst_id);
        let now = s.vol.GetMasterVolume().unwrap_or(target);
        if (now - target).abs() > EPS {
            s.vol.SetMasterVolume(target, &OUR_CONTEXT)?;
            let n = shared.restored.fetch_add(1, Ordering::Relaxed) + 1;
            let c = logged.entry(s.key.clone()).or_default();
            *c += 1;
            // в журнал — первый раз и дальше изредка: приложение может спорить часто
            if *c == 1 || *c % 50 == 0 {
                crate::util::log(format!(
                    "замок громкости: {} {:.0}% -> {:.0}% (раз для приложения: {c}, всего: {n})",
                    s.key,
                    now * 100.0,
                    target * 100.0
                ));
            }
        }
    }
    // закрытые сессии и снятые замки — отписываемся
    w.sessions.retain(|inst, (c, sink)| {
        let keep = alive.contains(inst);
        if !keep {
            let _ = c.UnregisterAudioSessionNotification(&*sink);
        }
        keep
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn apps_sorted_with_system_sounds_last() {
        let a = |key: &str, name: &str| AppVolume { key: key.into(), name: name.into(), volume: 1.0, muted: false, active: false };
        let mut v = vec![a(SYSTEM_KEY, "Системные звуки"), a("spotify.exe", "Spotify"), a("discord.exe", "discord")];
        sort_apps(&mut v);
        let names: Vec<_> = v.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["discord", "Spotify", "Системные звуки"]);
    }

    /// Что видит микшер на этом компьютере: `cargo test --release --lib print_mixer -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_mixer() {
        let m = mixer_state().unwrap();
        println!("{}: {:.0}% {}", m.device, m.master * 100.0, if m.master_muted { "(без звука)" } else { "" });
        for a in m.apps {
            println!("  {:<28} {:<24} {:>4.0}% {} {}", a.name, a.key, a.volume * 100.0, if a.muted { "без звука" } else { "" }, if a.active { "играет" } else { "" });
        }        for o in &m.outputs {
            println!("вывод: {}{}", o.name, if o.is_default { "  ← по умолчанию" } else { "" });
        }
        if let Some(mic) = &m.mic {
            println!("микрофон: {} {:.0}% {}", mic.name, mic.volume * 100.0, if mic.muted { "(без звука)" } else { "" });
        }
        for o in &m.mics {
            println!("микрофон в списке: {}{}", o.name, if o.is_default { "  ← по умолчанию" } else { "" });
        }
        // переключатель устройств есть в этой Windows (сам ничего не переключаем)
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let unk: windows::core::IUnknown =
                CoCreateInstance(&GUID::from_u128(0x870a_f99c_171d_4f9e_af0d_e63d_f40c_2bc9), None, CLSCTX_ALL).unwrap();
            let mut p = std::ptr::null_mut();
            assert!(unk.query(&GUID::from_u128(0xf867_9f50_850a_41cf_9c72_430f_2902_90c8), &mut p).is_ok());
            drop(windows::core::IUnknown::from_raw(p));
            println!("IPolicyConfig: есть");
        }
    }

    /// Свой звук в общем режиме (тишина) — у тестового процесса появляется аудиосессия.
    struct Silence {
        client: IAudioClient,
    }

    impl Silence {
        fn open() -> Option<Self> {
            unsafe {
                let dev = default_output().ok()?;
                let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).ok()?;
                let fmt = client.GetMixFormat().ok()?;
                let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, fmt, None);
                CoTaskMemFree(Some(fmt as *const _));
                r.ok()?;
                let render: IAudioRenderClient = client.GetService().ok()?;
                let frames = client.GetBufferSize().ok()?;
                render.GetBuffer(frames).ok()?;
                render.ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).ok()?;
                client.Start().ok()?;
                Some(Self { client })
            }
        }

        fn volume(&self) -> ISimpleAudioVolume {
            unsafe { self.client.GetService().unwrap() }
        }
    }

    impl Drop for Silence {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }

    fn wait_for(what: impl Fn() -> bool, limit: Duration) -> Option<Duration> {
        let t = Instant::now();
        while t.elapsed() < limit {
            if what() {
                return Some(t.elapsed());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// Замок на своём же процессе: чужая смена громкости (как Discord, сбрасывающий её на 100 %)
    /// откатывается по уведомлению Windows — быстрее запасной проверки раз в 2 с.
    #[test]
    fn lock_holds_volume() {
        let Some(snd) = Silence::open() else {
            eprintln!("нет устройства вывода звука — проверка замка пропущена");
            return;
        };
        let vol = snd.volume();
        let key = crate::util::exe_name(std::process::id()).unwrap().to_lowercase();
        let get = || unsafe { vol.GetMasterVolume().unwrap() };
        unsafe { vol.SetMasterVolume(1.0, std::ptr::null()).unwrap() };

        let keeper = VolumeKeeper::start();
        keeper.set_locks([(key.clone(), 0.3)]);
        assert!(wait_for(|| (get() - 0.3).abs() < EPS, Duration::from_secs(3)).is_some(), "замок не выставил громкость: {}", get());

        // «Discord» сам ставит 100 %
        for round in 0..3 {
            let before = keeper.notified();
            unsafe { vol.SetMasterVolume(1.0, std::ptr::null()).unwrap() };
            let took = wait_for(|| (get() - 0.3).abs() < EPS, Duration::from_secs(3));
            assert!(took.is_some(), "раунд {round}: громкость не вернулась, сейчас {}", get());
            assert!(keeper.notified() > before, "раунд {round}: вернула запасная проверка, а не уведомление");
            assert!(took.unwrap() < Duration::from_millis(1500), "раунд {round}: слишком долго: {:?}", took);
        }

        // замок снят — громкость больше не трогаем
        keeper.set_locks([]);
        std::thread::sleep(Duration::from_millis(100));
        unsafe { vol.SetMasterVolume(0.8, std::ptr::null()).unwrap() };
        std::thread::sleep(Duration::from_millis(2500));
        assert!((get() - 0.8).abs() < EPS, "без замка громкость изменилась: {}", get());
        unsafe { vol.SetMasterVolume(1.0, std::ptr::null()).unwrap() };
    }
}
