//! Нагрузка компьютера для оверлея: процессор, видеокарта, видеопамять, оперативная память.
//!
//! Процессор — `GetSystemTimes`, память — `GlobalMemoryStatusEx`. Видеокарта — счётчики
//! производительности Windows (PDH), те же, что у диспетчера задач:
//! `\GPU Engine(*)\Utilization Percentage` — по одному на каждый процесс и движок
//! (3D, копирование, кодирование видео…). Как в диспетчере задач, загрузку движка
//! складываем по всем процессам, а загрузка видеокарты — самый занятый движок.
//! Видеопамять — `\GPU Adapter Memory(*)\Dedicated Usage`. Права администратора не нужны.
//! Считаем только для основной видеокарты — той, у которой больше всего своей памяти.

use anyhow::{bail, Result};
use std::collections::HashMap;
use windows::core::w;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
use windows::Win32::System::Performance::*;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Load {
    /// Процессор, % (0–100).
    pub cpu: Option<f32>,
    /// Видеокарта, % самого занятого движка.
    pub gpu: Option<f32>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    pub ram_used: u64,
    pub ram_total: u64,
    pub gpu_name: String,
}

/// Счётчики открыты, пока жив; каждый `sample` — значения с прошлого вызова.
pub struct Sampler {
    query: PDH_HQUERY,
    engine: Option<PDH_HCOUNTER>,
    memory: Option<PDH_HCOUNTER>,
    /// «luid_0x00000000_0x0000c6d0» основной видеокарты (как в именах счётчиков).
    luid: String,
    vram_total: Option<u64>,
    gpu_name: String,
    cpu_prev: Option<(u64, u64)>,
}

// PDH-запрос — просто описатель; обращения к нему идут под мьютексом приложения
unsafe impl Send for Sampler {}

impl Sampler {
    pub fn new() -> Result<Self> {
        let (luid, vram_total, gpu_name) = main_gpu().unwrap_or_default();
        unsafe {
            let mut query = PDH_HQUERY::default();
            let r = PdhOpenQueryW(None, 0, &mut query);
            if r != 0 {
                bail!("счётчики производительности недоступны (PDH 0x{r:08X})");
            }
            let add = |path: windows::core::PCWSTR| {
                let mut c = PDH_HCOUNTER::default();
                (PdhAddEnglishCounterW(query, path, 0, &mut c) == 0).then_some(c)
            };
            let engine = add(w!("\\GPU Engine(*)\\Utilization Percentage"));
            let memory = add(w!("\\GPU Adapter Memory(*)\\Dedicated Usage"));
            PdhCollectQueryData(query);
            let mut s = Self { query, engine, memory, luid, vram_total: (vram_total > 0).then_some(vram_total), gpu_name, cpu_prev: None };
            s.cpu_prev = cpu_times();
            Ok(s)
        }
    }

    pub fn sample(&mut self) -> Load {
        let mut load = Load { gpu_name: self.gpu_name.clone(), vram_total: self.vram_total, ..Default::default() };
        let now = cpu_times();
        if let (Some((busy0, all0)), Some((busy1, all1))) = (self.cpu_prev, now) {
            if all1 > all0 {
                load.cpu = Some((busy1.saturating_sub(busy0) as f64 / (all1 - all0) as f64 * 100.0).clamp(0.0, 100.0) as f32);
            }
        }
        self.cpu_prev = now;
        unsafe {
            let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
            if GlobalMemoryStatusEx(&mut m).is_ok() {
                load.ram_total = m.ullTotalPhys;
                load.ram_used = m.ullTotalPhys.saturating_sub(m.ullAvailPhys);
            }
            if PdhCollectQueryData(self.query) == 0 {
                if let Some(items) = self.engine.and_then(|c| counter_array(c)) {
                    load.gpu = gpu_busy(&items, &self.luid).map(|v| v as f32);
                }
                if let Some(items) = self.memory.and_then(|c| counter_array(c)) {
                    let used: f64 = items.iter().filter(|(n, _)| n.to_lowercase().starts_with(&self.luid)).map(|(_, v)| v).sum();
                    if !self.luid.is_empty() && items.iter().any(|(n, _)| n.to_lowercase().starts_with(&self.luid)) {
                        load.vram_used = Some(used as u64);
                    }
                }
            }
        }
        load
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// (занято, всего) процессорного времени с запуска системы, в 100 нс.
fn cpu_times() -> Option<(u64, u64)> {
    let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
    let v = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    // время ядра включает простой
    let all = v(kernel) + v(user);
    Some((all.saturating_sub(v(idle)), all))
}

/// Основная видеокарта: (метка luid, своя память в байтах, название).
fn main_gpu() -> Option<(String, u64, String)> {
    unsafe {
        let f: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut best: Option<(String, u64, String)> = None;
        let mut i = 0;
        while let Ok(a) = f.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = a.GetDesc1() else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let mem = d.DedicatedVideoMemory as u64;
            if best.as_ref().is_none_or(|b| mem > b.1) {
                let luid = luid_tag(d.AdapterLuid.HighPart as u32, d.AdapterLuid.LowPart);
                best = Some((luid, mem, crate::gpu::wide_to_string(&d.Description)));
            }
        }
        best
    }
}

fn luid_tag(high: u32, low: u32) -> String {
    format!("luid_0x{high:08x}_0x{low:08x}")
}

/// Значения счётчика со звёздочкой: (имя экземпляра, значение).
unsafe fn counter_array(c: PDH_HCOUNTER) -> Option<Vec<(String, f64)>> {
    let fmt = PDH_FMT(PDH_FMT_DOUBLE.0 | 0x8000); // PDH_FMT_NOCAP100: сумма может быть больше 100
    let (mut size, mut count) = (0u32, 0u32);
    let r = PdhGetFormattedCounterArrayW(c, fmt, &mut size, &mut count, None);
    if r != PDH_MORE_DATA || size == 0 {
        return None;
    }
    // буфер из u64 — выравнивание для структур с double
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
    if PdhGetFormattedCounterArrayW(c, fmt, &mut size, &mut count, Some(items)) != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(count as usize);
    for it in std::slice::from_raw_parts(items, count as usize) {
        // 0 — PDH_CSTATUS_VALID_DATA, 1 — PDH_CSTATUS_NEW_DATA
        if it.FmtValue.CStatus > 1 {
            continue;
        }
        out.push((it.szName.to_string().unwrap_or_default(), it.FmtValue.Anonymous.doubleValue));
    }
    Some(out)
}

/// Загрузка видеокарты по экземплярам `GPU Engine`:
/// «pid_1234_luid_0x…_0x…_phys_0_eng_0_engtype_3D» -> движок «luid_…_eng_0_engtype_3D».
/// Складываем процессы по каждому движку и берём самый занятый движок.
fn gpu_busy(items: &[(String, f64)], luid: &str) -> Option<f64> {
    let mut engines: HashMap<String, f64> = HashMap::new();
    for (name, v) in items {
        let name = name.to_lowercase();
        let Some(at) = name.find("luid_") else { continue };
        let engine = &name[at..];
        if !luid.is_empty() && !engine.starts_with(luid) {
            continue;
        }
        *engines.entry(engine.to_string()).or_default() += v;
    }
    if engines.is_empty() {
        return None;
    }
    Some(engines.values().cloned().fold(0.0, f64::max).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_busy_is_the_busiest_engine_summed_over_processes() {
        let l = luid_tag(0, 0xC6D0);
        assert_eq!(l, "luid_0x00000000_0x0000c6d0");
        let items = vec![
            ("pid_100_luid_0x00000000_0x0000C6D0_phys_0_eng_0_engtype_3D".to_string(), 30.0),
            ("pid_200_luid_0x00000000_0x0000C6D0_phys_0_eng_0_engtype_3D".to_string(), 25.0),
            ("pid_200_luid_0x00000000_0x0000C6D0_phys_0_eng_3_engtype_VideoEncode".to_string(), 40.0),
            // встроенная видеокарта не считается
            ("pid_300_luid_0x00000000_0x0000AAAA_phys_0_eng_0_engtype_3D".to_string(), 90.0),
        ];
        assert_eq!(gpu_busy(&items, &l), Some(55.0));
        assert_eq!(gpu_busy(&items[..1], "luid_0x00000000_0x0000ffff"), None);
        let over = vec![("pid_1_luid_0x00000000_0x0000c6d0_phys_0_eng_0_engtype_3D".to_string(), 140.0)];
        assert_eq!(gpu_busy(&over, &l), Some(100.0));
    }

    /// Счётчики на этом компьютере: `cargo test --release --lib print_load -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_load() {
        let mut s = Sampler::new().unwrap();
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            println!("{:?}", s.sample());
        }
    }
}
