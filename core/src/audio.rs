//! Захват звука через WASAPI: системный звук (loopback — всё, что слышно в наушниках)
//! и микрофон. На выходе — непрерывная лента PCM 48 кГц стерео (f32) с позициями
//! на общих с видео часах: паузы в звуке заполняются тишиной, чтобы не было рассинхрона.

use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;

pub const RATE: u32 = 48_000;

/// Кусок звука одного источника: `pos` — номер первого кадра на общей шкале (48 кГц от старта записи).
pub struct Chunk {
    pub source: usize,
    pub pos: i64,
    /// Чередующиеся L, R.
    pub samples: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceKind {
    /// Всё, что воспроизводится на устройстве вывода.
    System,
    /// Микрофон; None — устройство по умолчанию.
    Mic(Option<String>),
    /// Звук одного приложения (по имени exe), со всеми его дочерними процессами.
    App(String),
}

/// Текущее время в единицах 100 нс по тем же часам, что и у WASAPI (QPC).
pub fn qpc_now() -> i64 {
    let mut c = 0i64;
    let mut f = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    (c as i128 * 10_000_000 / f.max(1) as i128) as i64
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    /// Выключен в Windows (или кнопкой на самом микрофоне, если драйвер это сообщает).
    pub muted: bool,
    /// Громкость записи в Windows, 0..1.
    pub volume: f32,
    /// Текущий уровень сигнала 0..1 (работает, пока кто-то пишет с микрофона).
    pub peak: f32,
}

/// Список микрофонов с громкостью и текущим уровнем.
pub fn list_mics() -> Result<Vec<Device>> {
    use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let def = en.GetDefaultAudioEndpoint(eCapture, eConsole).ok().and_then(|d| device_id(&d).ok());
        let col = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..col.GetCount()? {
            let d = col.Item(i)?;
            let id = device_id(&d)?;
            let vol: Option<IAudioEndpointVolume> = d.Activate(CLSCTX_ALL, None).ok();
            let meter: Option<IAudioMeterInformation> = d.Activate(CLSCTX_ALL, None).ok();
            out.push(Device {
                name: device_name(&d),
                is_default: def.as_deref() == Some(id.as_str()),
                muted: vol.as_ref().and_then(|v| v.GetMute().ok()).is_some_and(|m| m.as_bool()),
                volume: vol.as_ref().and_then(|v| v.GetMasterVolumeLevelScalar().ok()).unwrap_or(1.0),
                peak: meter.as_ref().and_then(|m| m.GetPeakValue().ok()).unwrap_or(0.0),
                id,
            });
        }
        Ok(out)
    }
}

pub(crate) unsafe fn device_id(d: &IMMDevice) -> Result<String> {
    let p = d.GetId()?;
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    Ok(s)
}

pub(crate) unsafe fn device_name(d: &IMMDevice) -> String {
    d.OpenPropertyStore(STGM_READ)
        .and_then(|ps| ps.GetValue(&PKEY_Device_FriendlyName))
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "Неизвестное устройство".into())
}

/// Формат устройства, который мы умеем читать.
#[derive(Clone, Copy, Debug)]
enum Fmt {
    F32,
    I16,
    I24,
    I32,
}

const WAVE_FORMAT_EXTENSIBLE: u32 = 0xFFFE;
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

unsafe fn read_format(p: *const WAVEFORMATEX) -> Result<(Fmt, u32, usize, usize)> {
    let wf = &*p;
    let ch = wf.nChannels as usize;
    let rate = wf.nSamplesPerSec;
    let bits = wf.wBitsPerSample;
    let tag = wf.wFormatTag as u32;
    let sub = if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = &*(p as *const WAVEFORMATEXTENSIBLE);
        let sf = ext.SubFormat;
        if sf == SUBTYPE_FLOAT {
            3
        } else if sf == SUBTYPE_PCM {
            1
        } else {
            0
        }
    } else {
        tag
    };
    let fmt = match (sub, bits) {
        (3, 32) => Fmt::F32,
        (1, 16) => Fmt::I16,
        (1, 24) => Fmt::I24,
        (1, 32) => Fmt::I32,
        _ => bail!("неподдерживаемый формат звука: тег {tag}, {bits} бит"),
    };
    Ok((fmt, rate, ch, wf.nBlockAlign as usize))
}

/// Перевод одного кадра устройства в стерео (L, R); 5.1/7.1 сводится в стерео.
fn frame_to_stereo(fmt: Fmt, data: &[u8], ch: usize) -> (f32, f32) {
    let s = |i: usize| -> f32 {
        match fmt {
            Fmt::F32 => f32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap()),
            Fmt::I16 => i16::from_le_bytes(data[i * 2..i * 2 + 2].try_into().unwrap()) as f32 / 32768.0,
            Fmt::I24 => {
                let b = &data[i * 3..i * 3 + 3];
                (((b[2] as i32) << 24 | (b[1] as i32) << 16 | (b[0] as i32) << 8) >> 8) as f32 / 8_388_608.0
            }
            Fmt::I32 => i32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap()) as f32 / 2_147_483_648.0,
        }
    };
    match ch {
        0 => (0.0, 0.0),
        1 => (s(0), s(0)),
        2 => (s(0), s(1)),
        // FL FR FC [LFE] [BL BR] [SL SR]: центр и тылы подмешиваем к своим сторонам
        _ => {
            let (mut l, mut r) = (s(0), s(1));
            if ch >= 3 {
                let c = s(2) * 0.707;
                l += c;
                r += c;
            }
            if ch >= 6 {
                l += s(4) * 0.5;
                r += s(5) * 0.5;
            }
            if ch >= 8 {
                l += s(6) * 0.5;
                r += s(7) * 0.5;
            }
            (l, r)
        }
    }
}

/// Линейный пересчёт частоты в 48 кГц (с сохранением состояния между кусками).
/// `corr` — поправка на уход часов устройства: +0,001 — выдать на 0,1 % меньше кадров.
pub struct Resampler {
    from: u32,
    phase: f64,
    prev: (f32, f32),
    corr: f64,
}

impl Resampler {
    pub fn new(from: u32) -> Self {
        Self { from, phase: 0.0, prev: (0.0, 0.0), corr: 0.0 }
    }

    pub fn set_correction(&mut self, corr: f64) {
        self.corr = corr;
    }

    pub fn process(&mut self, input: &[(f32, f32)], out: &mut Vec<f32>) {
        if self.from == RATE && self.corr == 0.0 {
            for &(l, r) in input {
                out.push(l);
                out.push(r);
            }
            // следующий выходной отсчёт — первый кадр следующего куска
            self.phase = 1.0;
            if let Some(&last) = input.last() {
                self.prev = last;
            }
            return;
        }
        let step = self.from as f64 / RATE as f64 * (1.0 + self.corr);
        // phase — позиция следующего выходного отсчёта относительно prev (0 = prev, 1 = input[0])
        let mut t = self.phase;
        let at = |i: isize, prev: (f32, f32)| if i < 0 { prev } else { input[i as usize] };
        while t < input.len() as f64 {
            let i = t.floor() as isize - 1;
            let frac = (t - t.floor()) as f32;
            let a = at(i, self.prev);
            let b = at(i + 1, self.prev);
            out.push(a.0 + (b.0 - a.0) * frac);
            out.push(a.1 + (b.1 - a.1) * frac);
            t += step;
        }
        self.phase = t - input.len() as f64;
        if let Some(&last) = input.last() {
            self.prev = last;
        }
    }
}

/// Расстановка кусков одного источника на общей шкале.
///
/// Метки времени устройства дрожат, а его часы чуть спешат или отстают от системных
/// (у USB-микрофонов это обычно десятки–сотни ppm). Поэтому кусок, пришедший «почти
/// вовремя», ставится вплотную к предыдущему — как в OBS, — а накопившийся уход часов
/// плавно убирается пересчётом частоты (`correction`). Вставляем тишину или выбрасываем
/// звук только при настоящем разрыве: устройство замолкало или поток надолго задержался.
///
/// Раньше допуск был 10 мс, а при выбрасывании счётчик ещё и сдвигался вперёд: стоило
/// часам микрофона уйти на 10 мс (у fifine — через 69 с), и дальше выбрасывалось всё.
#[derive(Default)]
pub struct Placer {
    /// Счётчики для журнала, кадры 48 кГц.
    pub stats: PlaceStats,
    /// Позиция, с которой встанет следующий кадр.
    next: Option<i64>,
    /// Сглаженное расхождение «наш счёт − метки устройства», кадры (>0 — мы впереди).
    err: f64,
    correcting: bool,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct PlaceStats {
    /// Ушло в запись звука устройства.
    pub sent: i64,
    /// Вставлено тишины (устройство молчало или задержалось).
    pub silence: i64,
    /// Выброшено (пришло «из прошлого»).
    pub dropped: i64,
}

impl Placer {
    /// Расхождение меньше этого — дрожание и уход часов, а не разрыв.
    pub const SMOOTH: i64 = RATE as i64 / 20; // 50 мс
    /// Выравнивать часы начинаем с 8 мс расхождения и заканчиваем на 1 мс.
    const CORRECT_ON: f64 = RATE as f64 * 0.008;
    const CORRECT_OFF: f64 = RATE as f64 * 0.001;
    /// Не больше 0,2 % — на слух незаметно (≈3 цента).
    const MAX_CORR: f64 = 0.002;

    pub fn new() -> Self {
        Self::default()
    }

    /// Поправка частоты для ресемплера (расхождение убирается примерно за 20 с).
    pub fn correction(&self) -> f64 {
        if self.correcting {
            (self.err / (20.0 * RATE as f64)).clamp(-Self::MAX_CORR, Self::MAX_CORR)
        } else {
            0.0
        }
    }

    /// Кусок (48 кГц, стерео), первый кадр которого по меткам устройства — на `start`.
    /// В `out` — что отправить: (позиция, звук).
    pub fn place(&mut self, start: i64, samples: Vec<f32>, out: &mut Vec<(i64, Vec<f32>)>) {
        let n = samples.len() as i64 / 2;
        let next = *self.next.get_or_insert(start);
        let d = start - next;
        // где на шкале стоит нулевой кадр куска
        let base = if d > Self::SMOOTH {
            // разрыв: устройство молчало — заполняем тишиной
            out.push((next, vec![0.0; d as usize * 2]));
            self.stats.silence += d;
            self.next = Some(start);
            self.reset_clock();
            start
        } else if d < -Self::SMOOTH {
            // звук «из прошлого» (это место уже заполнено): лишнее выбросим
            self.reset_clock();
            start
        } else {
            self.track_clock(-d as f64);
            next
        };
        let cur = self.next.unwrap_or(base);
        let first = cur.max(0); // до начала записи ничего не пишем
        let lead = first - base;
        let end = base + n;
        self.stats.dropped += (first - base).clamp(0, n);
        if end > first {
            self.stats.sent += end - first;
            let mut s = samples;
            if lead > 0 {
                s.drain(..lead as usize * 2);
            }
            out.push((first, s));
        }
        self.next = Some(cur.max(end));
    }

    /// Устройство давно ничего не присылает — дописать тишину до `target`.
    pub fn fill_to(&mut self, target: i64) -> Option<(i64, Vec<f32>)> {
        match self.next {
            None => {
                if target >= 0 {
                    self.next = Some(target);
                }
                None
            }
            Some(from) if target - from > RATE as i64 / 10 => {
                self.next = Some(target);
                self.stats.silence += target - from;
                Some((from, vec![0.0; (target - from) as usize * 2]))
            }
            _ => None,
        }
    }

    fn track_clock(&mut self, err: f64) {
        self.err += (err - self.err) * 0.05;
        let a = self.err.abs();
        if a > Self::CORRECT_ON {
            self.correcting = true;
        } else if a < Self::CORRECT_OFF {
            self.correcting = false;
        }
    }

    fn reset_clock(&mut self) {
        self.err = 0.0;
        self.correcting = false;
    }
}

/// Поток захвата одного источника. Работает, пока `stop` не выставлен.
/// При ошибке (отключили наушники, приложение закрыли) пытается открыть источник снова,
/// а пока его нет — держит ленту непрерывной тишиной.
pub fn spawn_source(index: usize, kind: SourceKind, t0: i64, out: Sender<Chunk>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("clipback-audio-{index}"))
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut placer = Placer::new();
            let mut diag = Diag::new();
            let mut last_err = String::new();
            while !stop.load(Ordering::Relaxed) {
                match run_source(index, &kind, t0, &out, &stop, &mut placer, &mut diag) {
                    Ok(()) => last_err.clear(),
                    Err(e) => {
                        // одну и ту же ошибку пишем в журнал один раз
                        let msg = format!("{e:#}");
                        if msg != last_err {
                            crate::util::log(format!("звук ({}): {msg}", kind.describe()));
                            last_err = msg;
                        }
                        let deadline = Instant::now() + Duration::from_millis(1500);
                        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(50));
                            fill_silence(index, t0, &out, &mut placer, 0);
                        }
                        if diag.since.elapsed() >= Diag::every() {
                            diag.report(&kind, &mut placer);
                        }
                    }
                }
            }
            unsafe { CoUninitialize() };
        })
        .expect("не удалось запустить поток звука")
}

impl SourceKind {
    pub fn describe(&self) -> String {
        match self {
            SourceKind::System => "звук системы".into(),
            SourceKind::Mic(None) => "микрофон по умолчанию".into(),
            SourceKind::Mic(Some(_)) => "выбранный микрофон".into(),
            SourceKind::App(exe) => format!("приложение {exe}"),
        }
    }
}

/// Микрофон не присылает данных дольше — переоткрываем поток.
/// (Тишину не считаем поломкой: человек молчит, шумодав глушит фон, кнопка «без звука».
/// Раньше тишина дольше минуты тоже вела к переоткрытию — это была догадка до того,
/// как в 0.4.4 нашлась настоящая причина пропадания микрофона, уход его часов.)
const MIC_STALL: Duration = Duration::from_secs(2);

/// Переоткрытие микрофона — в журнал, но не чаще раза в 10 минут.
fn note_reopen() {
    static LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let mut l = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if l.is_none_or(|t| t.elapsed() > Duration::from_secs(600)) {
        *l = Some(Instant::now());
        crate::util::log("микрофон перестал присылать звук — переоткрываю");
    }
}

fn pos_of(qpc: i64, t0: i64) -> i64 {
    ((qpc - t0) as i128 * RATE as i128 / 10_000_000) as i64
}

/// Если данных давно не было (в loopback тишина приходит «ничем»), дописываем нули
/// до «сейчас − 50 мс − обычная задержка устройства»: настоящий звук, который ещё в пути,
/// не должен оказаться «в прошлом».
fn fill_silence(index: usize, t0: i64, out: &Sender<Chunk>, placer: &mut Placer, lag: i64) {
    let target = pos_of(qpc_now(), t0) - RATE as i64 / 20 - lag;
    if let Some((pos, samples)) = placer.fill_to(target) {
        let _ = out.send(Chunk { source: index, pos, samples });
    }
}

// ---------- захват отдельного приложения (process loopback, Windows 10 2004+) ----------

#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(std::sync::mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

/// PROPVARIANT с типом VT_BLOB — так передаются параметры захвата приложения.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    r1: u16,
    r2: u16,
    r3: u16,
    size: u32,
    data: *const u8,
}

/// Процесс по имени exe. Если их несколько (как у Discord), берём главный — у которого родитель другой.
pub fn find_process(exe: &str) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    let mut all = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let name = crate::gpu::wide_to_string(&e.szExeFile);
                all.push((e.th32ProcessID, e.th32ParentProcessID, name));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snap);
    }
    let same: Vec<_> = all.iter().filter(|(_, _, n)| n.eq_ignore_ascii_case(exe)).collect();
    same.iter()
        .find(|(_, parent, _)| !same.iter().any(|(pid, _, _)| pid == parent))
        .or(same.first())
        .map(|(pid, _, _)| *pid)
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioApp {
    pub exe: String,
    pub name: String,
}

/// Приложения, которые сейчас выводят (или недавно выводили) звук — для выбора дорожек.
pub fn list_audio_apps() -> Result<Vec<AudioApp>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
        let list = mgr.GetSessionEnumerator()?;
        let me = std::process::id();
        let mut out: Vec<AudioApp> = Vec::new();
        for i in 0..list.GetCount()? {
            let Ok(ctrl) = list.GetSession(i) else { continue };
            let Ok(c2) = windows::core::Interface::cast::<IAudioSessionControl2>(&ctrl) else { continue };
            let pid = c2.GetProcessId().unwrap_or(0);
            if pid == 0 || pid == me {
                continue;
            }
            let Some(exe) = crate::util::exe_name(pid) else { continue };
            if out.iter().any(|a| a.exe.eq_ignore_ascii_case(&exe)) {
                continue;
            }
            let stem = exe.trim_end_matches(".exe").trim_end_matches(".EXE");
            let mut name: String = stem.chars().take(1).flat_map(|c| c.to_uppercase()).chain(stem.chars().skip(1)).collect();
            if name.is_empty() {
                name = exe.clone();
            }
            out.push(AudioApp { exe, name });
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }
}

unsafe fn activate_app(pid: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let blob = BlobVariant {
        vt: 65, // VT_BLOB
        r1: 0,
        r2: 0,
        r3: 0,
        size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        data: &params as *const _ as *const u8,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    let op = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &<IAudioClient as windows::core::Interface>::IID,
        Some(&blob as *const BlobVariant as *const _),
        &handler,
    )?;
    rx.recv_timeout(Duration::from_secs(5)).context("Windows не ответил на захват приложения")?;
    let mut hr = windows::core::HRESULT(0);
    let mut unk = None;
    op.GetActivateResult(&mut hr, &mut unk)?;
    hr.ok().context("захват приложения недоступен (нужна Windows 10 2004 или новее)")?;
    let unk = unk.context("пустой ответ Windows")?;
    Ok(windows::core::Interface::cast(&unk)?)
}

// ---------- общий цикл чтения ----------

/// Запас буфера устройства: 1 с. Задержку он не добавляет, зато если поток записи
/// на мгновение придушила игра, звук дождётся нас, а не пропадёт.
const BUFFER_100NS: i64 = 10_000_000;

struct Opened {
    client: IAudioClient,
    fmt: Fmt,
    rate: u32,
    ch: usize,
    block: usize,
    dev_id: String,
    event: Option<windows::Win32::Foundation::HANDLE>,
    pid: u32,
}

unsafe fn open(kind: &SourceKind, en: &IMMDeviceEnumerator) -> Result<Opened> {
    use windows::Win32::System::Threading::CreateEventW;
    if let SourceKind::App(exe) = kind {
        let pid = find_process(exe).with_context(|| format!("{exe} не запущено — дорожка включится, когда оно запустится"))?;
        let client = activate_app(pid)?;
        // для захвата приложения формат задаём сами: 48 кГц, стерео, float
        let wf = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                BUFFER_100NS,
                0,
                &wf,
                None,
            )
            .context("захват приложения не открылся")?;
        let ev = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(ev)?;
        return Ok(Opened { client, fmt: Fmt::F32, rate: RATE, ch: 2, block: 8, dev_id: String::new(), event: Some(ev), pid });
    }
    let (device, flow) = match kind {
        SourceKind::System => (en.GetDefaultAudioEndpoint(eRender, eConsole)?, eRender),
        SourceKind::Mic(None) => (en.GetDefaultAudioEndpoint(eCapture, eConsole).context("в Windows нет микрофона")?, eCapture),
        SourceKind::Mic(Some(id)) => {
            let w: Vec<u16> = id.encode_utf16().chain([0]).collect();
            (en.GetDevice(PCWSTR(w.as_ptr())).context("выбранный микрофон не найден — отключён?")?, eCapture)
        }
        SourceKind::App(_) => unreachable!(),
    };
    let dev_id = device_id(&device).unwrap_or_default();
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let mix = client.GetMixFormat()?;
    let parsed = read_format(mix);
    let flags = if flow == eRender { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
    let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_100NS, 0, mix, None);
    CoTaskMemFree(Some(mix as *const _));
    let (fmt, rate, ch, block) = parsed?;
    init.context("устройство не открылось — возможно, его занимает другая программа")?;
    crate::util::log(format!("звук ({}): открыт «{}», {rate} Гц, каналов {ch}, {fmt:?}", kind.describe(), device_name(&device)));
    Ok(Opened { client, fmt, rate, ch, block, dev_id, event: None, pid: 0 })
}

/// Сводка об источнике звука раз в 5 минут — по ней видно, на каком шаге пропадает звук:
/// не приходит от устройства, приходит нулями, выбрасывается при расстановке или тонет в тишине.
pub struct Diag {
    since: Instant,
    /// Кадров от устройства (в его частоте) и сама частота.
    frames: u64,
    rate: u32,
    packets: u64,
    /// Пакеты с флагом «тишина» от Windows.
    silent_packets: u64,
    /// Пик сырого сигнала до усиления.
    peak: f32,
    reopens: u32,
}

impl Diag {
    /// Раз в 5 минут (CLIPBACK_DIAG_SECS — чаще, для проверки).
    pub fn every() -> Duration {
        static E: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
        *E.get_or_init(|| Duration::from_secs(std::env::var("CLIPBACK_DIAG_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300)))
    }

    pub fn new() -> Self {
        Self { since: Instant::now(), frames: 0, rate: RATE, packets: 0, silent_packets: 0, peak: 0.0, reopens: 0 }
    }

    fn report(&mut self, kind: &SourceKind, p: &mut Placer) {
        let secs = |f: i64| f as f64 / RATE as f64;
        let st = std::mem::take(&mut p.stats);
        let peak_db = if self.peak > 0.0 { format!("{:.0} дБ", 20.0 * self.peak.log10()) } else { "ноль".into() };
        crate::util::log(format!(
            "звук ({}) за {:.0} мин: от устройства {:.1} с в {} пакетах (с пометкой «тишина» {}), пик {:.4} ({peak_db}); в запись {:.1} с, вставлено тишины {:.1} с, выброшено {:.1} с, поправка часов {:+.0} ppm, переоткрытий {}",
            kind.describe(),
            self.since.elapsed().as_secs_f64() / 60.0,
            self.frames as f64 / self.rate.max(1) as f64,
            self.packets,
            self.silent_packets,
            self.peak,
            secs(st.sent),
            secs(st.silence),
            secs(st.dropped),
            p.correction() * 1e6,
            self.reopens,
        ));
        *self = Self { reopens: 0, ..Self::new() };
    }
}

impl Default for Diag {
    fn default() -> Self {
        Self::new()
    }
}

/// Метка времени первого кадра куска (100 нс, QPC). Бывает, что её нет (захват приложения)
/// или драйвер присылает ерунду — тогда считаем от момента, когда кусок до нас дошёл.
pub fn sane_time(qpc: i64, frames: u32, rate: u32, now: i64) -> i64 {
    let est = now - frames as i64 * 10_000_000 / rate.max(1) as i64;
    if qpc <= 0 || qpc > now + 500_000 || qpc < now - 30_000_000 {
        est
    } else {
        qpc
    }
}

fn run_source(index: usize, kind: &SourceKind, t0: i64, out: &Sender<Chunk>, stop: &AtomicBool, placer: &mut Placer, diag: &mut Diag) -> Result<()> {
    use windows::Win32::System::Threading::WaitForSingleObject;
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let o = open(kind, &en)?;
        if let SourceKind::App(exe) = kind {
            crate::util::log(format!("звук: пишу приложение {exe} (pid {})", o.pid));
        }
        let cap: IAudioCaptureClient = o.client.GetService()?;
        o.client.Start()?;

        let mut rs = Resampler::new(o.rate);
        diag.rate = o.rate;
        let mut frames_buf: Vec<(f32, f32)> = Vec::with_capacity(4800);
        let mut last_check = Instant::now();
        // микрофон иногда «замолкает»: поток открыт, ошибок нет, а данных нет (USB уснул).
        // Тогда переоткрываем поток — то, что помогало при повторном выборе микрофона.
        let is_mic = matches!(kind, SourceKind::Mic(_));
        let mut last_packet = Instant::now();
        // обычная задержка: насколько конец куска отстаёт от «сейчас», кадры 48 кГц
        let mut lag: Option<i64> = None;
        let mut placed = Vec::new();

        let r = (|| -> Result<()> {
            while !stop.load(Ordering::Relaxed) {
                let mut got = false;
                loop {
                    let n = cap.GetNextPacketSize()?;
                    if n == 0 {
                        break;
                    }
                    got = true;
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut fl = 0u32;
                    let mut qpc = 0u64;
                    cap.GetBuffer(&mut data, &mut frames, &mut fl, None, Some(&mut qpc))?;
                    frames_buf.clear();
                    if fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        frames_buf.resize(frames as usize, (0.0, 0.0));
                    } else {
                        let bytes = std::slice::from_raw_parts(data, frames as usize * o.block);
                        for f in bytes.chunks_exact(o.block) {
                            frames_buf.push(frame_to_stereo(o.fmt, f, o.ch));
                        }
                    }
                    cap.ReleaseBuffer(frames)?;
                    diag.packets += 1;
                    diag.frames += frames as u64;
                    if fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        diag.silent_packets += 1;
                    }
                    for &(l, r) in &frames_buf {
                        diag.peak = diag.peak.max(l.abs()).max(r.abs());
                    }
                    last_packet = Instant::now();

                    let mut samples = Vec::with_capacity(frames as usize * 2 + 8);
                    rs.set_correction(placer.correction());
                    rs.process(&frames_buf, &mut samples);
                    let now = qpc_now();
                    let qpc = sane_time(qpc as i64, frames, o.rate, now);
                    let end = pos_of(qpc + frames as i64 * 10_000_000 / o.rate as i64, t0);
                    let now_lag = pos_of(now, t0) - end;
                    lag = Some(lag.map_or(now_lag, |l| l + (now_lag - l) / 20));
                    placed.clear();
                    placer.place(pos_of(qpc, t0), samples, &mut placed);
                    for (pos, samples) in placed.drain(..) {
                        let _ = out.send(Chunk { source: index, pos, samples });
                    }
                }
                // тишину дописываем, только если устройство и правда давно молчит
                if !got && last_packet.elapsed() > Duration::from_millis(250) {
                    fill_silence(index, t0, out, placer, lag.unwrap_or(0).clamp(0, RATE as i64 / 2));
                }
                if !got {
                    match o.event {
                        Some(ev) => {
                            WaitForSingleObject(ev, 10);
                        }
                        None => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
                if is_mic {
                    if last_packet.elapsed() > MIC_STALL {
                        note_reopen();
                        diag.reopens += 1;
                        return Ok(());
                    }
                }
                if diag.since.elapsed() >= Diag::every() {
                    diag.report(kind, placer);
                }
                if last_check.elapsed() > Duration::from_secs(2) {
                    last_check = Instant::now();
                    match kind {
                        // сменилось устройство по умолчанию (подключили наушники) — переоткрываемся
                        SourceKind::System | SourceKind::Mic(None) => {
                            let flow = if *kind == SourceKind::System { eRender } else { eCapture };
                            if let Ok(d) = en.GetDefaultAudioEndpoint(flow, eConsole) {
                                if device_id(&d).unwrap_or_default() != o.dev_id {
                                    return Ok(());
                                }
                            }
                        }
                        // приложение перезапустили — цепляемся к новому процессу
                        SourceKind::App(exe) => {
                            if find_process(exe) != Some(o.pid) {
                                crate::util::log(format!("звук: {exe} закрыто или перезапущено"));
                                return Ok(());
                            }
                        }
                        SourceKind::Mic(Some(_)) => {}
                    }
                }
            }
            Ok(())
        })();
        let _ = o.client.Stop();
        if let Some(ev) = o.event {
            let _ = windows::Win32::Foundation::CloseHandle(ev);
        }
        r
    }
}

#[cfg(test)]
#[path = "tests/audio.rs"]
mod tests;
