//! Выбор монитора и создание D3D11-устройства на той видеокарте, к которой он подключён.

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1};

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub output: IDXGIOutput1,
    pub adapter_name: String,
    pub monitor_name: String,
    pub width: u32,
    pub height: u32,
}

/// Список мониторов: (номер, название, ширина, высота).
pub fn list_monitors() -> Result<Vec<(usize, String, u32, u32)>> {
    let mut out = Vec::new();
    for (i, (_, output)) in enum_outputs()?.into_iter().enumerate() {
        let d = unsafe { output.GetDesc()? };
        let r = d.DesktopCoordinates;
        out.push((i, wide_to_string(&d.DeviceName), (r.right - r.left) as u32, (r.bottom - r.top) as u32));
    }
    Ok(out)
}

fn enum_outputs() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput1)>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut res = Vec::new();
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        let mut o = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            let desc = unsafe { output.GetDesc()? };
            if desc.AttachedToDesktop.as_bool() {
                res.push((adapter.clone(), output.cast::<IDXGIOutput1>()?));
            }
            o += 1;
        }
        a += 1;
    }
    Ok(res)
}

pub fn open(monitor: usize) -> Result<Gpu> {
    let outputs = enum_outputs()?;
    if outputs.is_empty() {
        bail!("не найдено ни одного монитора");
    }
    let (adapter, output) = outputs
        .into_iter()
        .nth(monitor)
        .with_context(|| format!("монитор {monitor} не найден"))?;

    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("не удалось создать D3D11-устройство")?;
    }
    let device = device.unwrap();
    let context = context.unwrap();

    // Приоритет на видеокарте — обычный, как у любой программы: игра важнее записи.
    // Раньше просили «высокий» класс, и под нагрузкой Clipback отнимал время у игры.
    // кодировщик Media Foundation работает с устройством из своего потока
    unsafe {
        let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
    }

    let adesc = unsafe { adapter.GetDesc1()? };
    let odesc = unsafe { output.GetDesc()? };
    let r = odesc.DesktopCoordinates;
    Ok(Gpu {
        device,
        context,
        output,
        adapter_name: wide_to_string(&adesc.Description),
        monitor_name: wide_to_string(&odesc.DeviceName),
        width: (r.right - r.left) as u32,
        height: (r.bottom - r.top) as u32,
    })
}

pub fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}
