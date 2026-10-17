//! Настройка драйвера NVIDIA «Метод представления Vulkan/OpenGL» для одной программы.
//!
//! На Windows 11 24H2+ драйвер выводит OpenGL- и Vulkan-игру на весь экран прямо на монитор,
//! мимо Windows, и захват экрана (Clipback, «Захват экрана» в OBS, трансляция Discord) получает
//! кадр раз в несколько секунд. Значение «Использовать цепочку буферов DXGI» выводит такую
//! игру через Windows, как DirectX-игры, — и запись работает. Меняем его только в профиле
//! программы игры (например, javaw.exe), не для всех игр — как NVIDIA Profile Inspector,
//! через NVAPI DRS (`nvapi64.dll`). Номера функций и устройство структур — из официального
//! NVAPI SDK (nvapi.h, nvapi_interface.h, NvApiDriverSettings.h).
//!
//! Сохранять настройки драйвера можно только с правами администратора: если их нет,
//! Clipback перезапускает себя с повышением и аргументом `--nvidia-dxgi <exe>` (см. `run_task`).

use std::ffi::c_void;

/// OGL_CPL_PREFER_DXPRESENT — «Vulkan/OpenGL present method».
const SETTING_ID: u32 = 0x20D6_90F8;
/// Значения настройки: 0 — собственный метод, 1 — цепочка буферов DXGI, 2 — авто (по умолчанию).
pub const PREFER_DXGI: u32 = 1;
const AUTO: u32 = 2;

const NVAPI_OK: i32 = 0;
const NVAPI_INVALID_USER_PRIVILEGE: i32 = -137;
const NVAPI_SETTING_NOT_FOUND: i32 = -160;
const NVAPI_PROFILE_NOT_FOUND: i32 = -163;
const NVAPI_EXECUTABLE_NOT_FOUND: i32 = -166;

const STR: usize = 2048; // NVAPI_UNICODE_STRING_MAX
type UStr = [u16; STR];
type Handle = *mut c_void;

#[repr(C)]
struct Setting {
    version: u32,
    name: UStr,
    id: u32,
    kind: u32,
    location: u32,
    is_current_predefined: u32,
    is_predefined_valid: u32,
    predefined: [u8; 4100],
    current: [u8; 4100],
}

#[repr(C)]
struct Application {
    version: u32,
    is_predefined: u32,
    app_name: UStr,
    friendly_name: UStr,
    launcher: UStr,
    file_in_folder: UStr,
    flags: u32,
    command_line: UStr,
}

#[repr(C)]
struct Profile {
    version: u32,
    name: UStr,
    gpu_support: u32,
    is_predefined: u32,
    num_apps: u32,
    num_settings: u32,
}

const fn ver<T>(v: u32) -> u32 {
    std::mem::size_of::<T>() as u32 | (v << 16)
}

fn ustr(s: &str) -> Box<UStr> {
    let mut out = Box::new([0u16; STR]);
    for (d, c) in out.iter_mut().zip(s.encode_utf16().take(STR - 1)) {
        *d = c;
    }
    out
}

fn zeroed<T>() -> Box<T> {
    // структуры NVAPI — только числа и массивы, нули для них допустимы
    unsafe { Box::new(std::mem::zeroed()) }
}

type Fn0 = unsafe extern "C" fn() -> i32;
type FnH = unsafe extern "C" fn(Handle) -> i32;
type FnPH = unsafe extern "C" fn(*mut Handle) -> i32;
type FnHPH = unsafe extern "C" fn(Handle, *mut Handle) -> i32;
type FindApp = unsafe extern "C" fn(Handle, *const u16, *mut Handle, *mut Application) -> i32;
type GetSetting = unsafe extern "C" fn(Handle, Handle, u32, *mut Setting) -> i32;
type SetSetting = unsafe extern "C" fn(Handle, Handle, *mut Setting) -> i32;
type CreateProfile = unsafe extern "C" fn(Handle, *mut Profile, *mut Handle) -> i32;
type CreateApp = unsafe extern "C" fn(Handle, Handle, *mut Application) -> i32;

/// Открытая сессия настроек драйвера.
struct Drs {
    session: Handle,
    destroy: FnH,
    save: FnH,
    find_app: FindApp,
    base_profile: FnHPH,
    get: GetSetting,
    set: SetSetting,
    create_profile: CreateProfile,
    create_app: CreateApp,
}

impl Drop for Drs {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.session);
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// Нет драйвера NVIDIA (другая видеокарта).
    NoNvidia,
    /// Сохранять настройки драйвера можно только с правами администратора.
    NeedAdmin,
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoNvidia => write!(f, "нет драйвера NVIDIA"),
            Error::NeedAdmin => write!(f, "нужны права администратора"),
            Error::Failed(s) => write!(f, "{s}"),
        }
    }
}

fn check(r: i32, what: &str) -> Result<(), Error> {
    match r {
        NVAPI_OK => Ok(()),
        NVAPI_INVALID_USER_PRIVILEGE => Err(Error::NeedAdmin),
        _ => Err(Error::Failed(format!("NVAPI: {what} — код {r}"))),
    }
}

type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;

/// `nvapi_QueryInterface` из драйвера после NvAPI_Initialize; None — драйвера NVIDIA нет.
fn nvapi() -> Option<QueryInterface> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    unsafe {
        let lib = LoadLibraryW(w!("nvapi64.dll")).ok()?;
        let qi = GetProcAddress(lib, s!("nvapi_QueryInterface"))?;
        let qi = std::mem::transmute_copy::<_, QueryInterface>(&qi);
        let init: Fn0 = func(qi, 0x0150_E828).ok()?;
        (init() == NVAPI_OK).then_some(qi)
    }
}

/// Функция NVAPI по номеру; нулевой адрес — такой функции нет в этом драйвере.
fn func<F: Copy>(qi: QueryInterface, id: u32) -> Result<F, Error> {
    assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
    let p = unsafe { qi(id) };
    if p.is_null() {
        return Err(Error::Failed(format!("в драйвере нет функции NVAPI 0x{id:08X}")));
    }
    // F — указатель на функцию той же сигнатуры, что в nvapi.h
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
}

/// NV_GPU_THERMAL_SETTINGS_V2
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ThermalSensor {
    controller: i32,
    default_min: i32,
    default_max: i32,
    current: i32,
    target: i32,
}

#[repr(C)]
#[derive(Default)]
struct ThermalSettings {
    version: u32,
    count: u32,
    sensor: [ThermalSensor; 3],
}

/// Температура первой видеокарты NVIDIA, °C. Прав администратора не нужно;
/// None — карта не NVIDIA или драйвер не ответил.
pub fn gpu_temperature() -> Option<i32> {
    type EnumGpus = unsafe extern "C" fn(*mut [Handle; 64], *mut u32) -> i32;
    type GetThermal = unsafe extern "C" fn(Handle, u32, *mut ThermalSettings) -> i32;
    static API: std::sync::OnceLock<Option<(EnumGpus, GetThermal)>> = std::sync::OnceLock::new();
    let (enum_gpus, get_thermal) = (*API.get_or_init(|| {
        let qi = nvapi()?;
        Some((func(qi, 0xE5AC_921F).ok()?, func(qi, 0xE364_0A56).ok()?))
    }))?;
    unsafe {
        let mut gpus = [std::ptr::null_mut(); 64];
        let mut n = 0u32;
        if enum_gpus(&mut gpus, &mut n) != NVAPI_OK || n == 0 {
            return None;
        }
        let mut t = ThermalSettings { version: ver::<ThermalSettings>(2), ..Default::default() };
        // 15 — NVAPI_THERMAL_TARGET_ALL
        if get_thermal(gpus[0], 15, &mut t) != NVAPI_OK {
            return None;
        }
        // датчик 1 — сам графический процессор (NVAPI_THERMAL_TARGET_GPU)
        t.sensor.iter().take(t.count.min(3) as usize).find(|s| s.target == 1).or(t.sensor.first()).map(|s| s.current).filter(|c| (1..150).contains(c))
    }
}

impl Drs {
    fn open() -> Result<Self, Error> {
        unsafe {
            let qi = nvapi().ok_or(Error::NoNvidia)?;
            let create: FnPH = func(qi, 0x0694_D52E)?;
            let load: FnH = func(qi, 0x375D_BD6B)?;
            let mut session: Handle = std::ptr::null_mut();
            check(create(&mut session), "сессия настроек")?;
            let drs = Drs {
                session,
                destroy: func(qi, 0xDAD9_CFF8)?,
                save: func(qi, 0xFCBC_7E14)?,
                find_app: func(qi, 0xEEE5_66B2)?,
                base_profile: func(qi, 0xDA84_66A0)?,
                get: func(qi, 0x73BF_8338)?,
                set: func(qi, 0x577D_D202)?,
                create_profile: func(qi, 0xCC17_6068)?,
                create_app: func(qi, 0x4347_A9DE)?,
            };
            check(load(drs.session), "чтение настроек")?;
            Ok(drs)
        }
    }

    /// Профиль, в котором есть эта программа (None — её нет ни в одном профиле).
    fn app_profile(&self, exe: &str) -> Result<Option<Handle>, Error> {
        let name = ustr(exe);
        let mut app = zeroed::<Application>();
        app.version = ver::<Application>(4);
        let mut h: Handle = std::ptr::null_mut();
        match unsafe { (self.find_app)(self.session, name.as_ptr(), &mut h, &mut *app) } {
            NVAPI_OK => Ok(Some(h)),
            NVAPI_EXECUTABLE_NOT_FOUND | NVAPI_PROFILE_NOT_FOUND => Ok(None),
            r => check(r, "поиск программы").map(|_| None),
        }
    }

    /// Значение настройки в профиле (с учётом общих настроек); None — не задана нигде.
    fn value(&self, profile: Handle) -> Result<Option<u32>, Error> {
        let mut s = zeroed::<Setting>();
        s.version = ver::<Setting>(1);
        match unsafe { (self.get)(self.session, profile, SETTING_ID, &mut *s) } {
            NVAPI_OK => Ok(Some(u32::from_le_bytes(s.current[..4].try_into().unwrap_or_default()))),
            NVAPI_SETTING_NOT_FOUND => Ok(None),
            r => check(r, "чтение настройки").map(|_| None),
        }
    }

    fn global(&self) -> Result<Handle, Error> {
        let mut h: Handle = std::ptr::null_mut();
        check(unsafe { (self.base_profile)(self.session, &mut h) }, "общий профиль")?;
        Ok(h)
    }
}

/// Какой метод вывода драйвер применит к этой программе (1 — DXGI, 0 — собственный, 2 — авто).
pub fn present_method(exe: &str) -> Result<u32, Error> {
    let drs = Drs::open()?;
    if let Some(p) = drs.app_profile(exe)? {
        if let Some(v) = drs.value(p)? {
            return Ok(v);
        }
    }
    Ok(drs.value(drs.global()?)?.unwrap_or(AUTO))
}

/// Включить «Использовать цепочку буферов DXGI» для программы `exe` (только для неё).
/// Нет программы ни в одном профиле — создаётся профиль «Clipback: <exe>».
pub fn enable_dxgi(exe: &str) -> Result<(), Error> {
    let drs = Drs::open()?;
    let profile = match drs.app_profile(exe)? {
        Some(p) => p,
        None => unsafe {
            let mut p = zeroed::<Profile>();
            p.version = ver::<Profile>(1);
            p.name.copy_from_slice(&*ustr(&format!("Clipback: {exe}")));
            let mut h: Handle = std::ptr::null_mut();
            check((drs.create_profile)(drs.session, &mut *p, &mut h), "создание профиля")?;
            let mut app = zeroed::<Application>();
            app.version = ver::<Application>(4);
            app.app_name.copy_from_slice(&*ustr(exe));
            check((drs.create_app)(drs.session, h, &mut *app), "добавление программы в профиль")?;
            h
        },
    };
    let mut s = zeroed::<Setting>();
    s.version = ver::<Setting>(1);
    s.id = SETTING_ID;
    s.kind = 0; // NVDRS_DWORD_TYPE
    s.current[..4].copy_from_slice(&PREFER_DXGI.to_le_bytes());
    check(unsafe { (drs.set)(drs.session, profile, &mut *s) }, "запись настройки")?;
    check(unsafe { (drs.save)(drs.session) }, "сохранение настроек")
}

/// Включить настройку, при необходимости — перезапустив Clipback с правами администратора
/// (Windows спросит разрешения). Ждёт, пока повышенная копия закончит.
pub fn enable_dxgi_elevating(exe: &str) -> Result<(), String> {
    match enable_dxgi(exe) {
        Ok(()) => return Ok(()),
        Err(Error::NeedAdmin) => {}
        Err(e) => return Err(e.to_string()),
    }
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let file = HSTRING::from(me.as_os_str());
    let params = HSTRING::from(format!("--nvidia-dxgi \"{exe}\""));
    let verb = HSTRING::from("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|_| "Windows не дала прав администратора — настройка не изменена".to_string())?;
        if info.hProcess.is_invalid() {
            return Err("не удалось запустить Clipback с правами администратора".into());
        }
        let _ = std::fs::remove_file(task_error_file());
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        match code {
            0 => Ok(()),
            _ => Err(std::fs::read_to_string(task_error_file()).unwrap_or_else(|_| format!("драйвер NVIDIA не принял настройку (код {code})"))),
        }
    }
}

/// Сюда копия с правами администратора пишет текст ошибки — его покажет основная программа.
fn task_error_file() -> std::path::PathBuf {
    std::env::temp_dir().join("clipback-nvidia-error.txt")
}

/// `--nvidia-dxgi <exe>`: копия Clipback с правами администратора меняет настройку и выходит.
/// Возвращает код выхода, если это была такая копия.
pub fn run_task(args: &[String]) -> Option<i32> {
    let i = args.iter().position(|a| a == "--nvidia-dxgi")?;
    let exe = args.get(i + 1)?;
    Some(match enable_dxgi(exe) {
        Ok(()) => 0,
        Err(e) => {
            let _ = std::fs::write(task_error_file(), e.to_string());
            2
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_nvapi_sdk() {
        // NVDRS_SETTING_VER = 0x13020, NVDRS_APPLICATION_VER_V4 = 0x45008 (sizeof 20488),
        // NVDRS_PROFILE_VER = 0x11014 — размеры из nvapi.h
        assert_eq!(ver::<Setting>(1), 0x0001_3020);
        assert_eq!(std::mem::size_of::<Application>(), 4 + 4 + 4 * 4096 + 4 + 4096);
        assert_eq!(ver::<Profile>(1), 0x0001_1014);
        // NV_GPU_THERMAL_SETTINGS_VER_2 = MAKE_NVAPI_VERSION(NV_GPU_THERMAL_SETTINGS_V2, 2), sizeof 68
        assert_eq!(ver::<ThermalSettings>(2), 0x0002_0044);
    }

    /// Температура видеокарты без прав администратора (на компьютере без NVIDIA — None).
    #[test]
    #[ignore]
    fn read_gpu_temperature() {
        println!("температура видеокарты: {:?}", gpu_temperature());
    }

    /// Только чтение: какой метод вывода драйвер применит к javaw.exe (Minecraft).
    #[test]
    #[ignore]
    fn read_present_method_for_java() {
        match present_method("javaw.exe") {
            Ok(v) => println!("javaw.exe: метод вывода Vulkan/OpenGL = {v} (1 — DXGI, 0 — собственный, 2 — авто)"),
            Err(e) => println!("javaw.exe: {e}"),
        }
    }
}
