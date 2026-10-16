//! Диагностика захвата: одновременно Desktop Duplication и Windows.Graphics.Capture
//! на одном мониторе. Раз в секунду печатает, сколько кадров пришло от каждого способа,
//! менялась ли картинка в центре экрана и какое окно сейчас на переднем плане.
//!
//!   cargo run --release --example capture_probe -- [секунд] [монитор] [задержка, с]

use anyhow::{Context, Result};
use clipback_core::gpu;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

const SAMPLE: u32 = 128;

/// Центр кадра -> маленькая текстура для чтения процессором -> хэш.
struct Sampler {
    staging: ID3D11Texture2D,
}

impl Sampler {
    fn new(dev: &ID3D11Device) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: SAMPLE,
            Height: SAMPLE,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t))? };
        Ok(Self { staging: t.context("staging")? })
    }

    fn hash(&self, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D) -> Result<u64> {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut d) };
        let (x, y) = ((d.Width.saturating_sub(SAMPLE)) / 2, (d.Height.saturating_sub(SAMPLE)) / 2);
        let b = D3D11_BOX { left: x, top: y, front: 0, right: x + SAMPLE, bottom: y + SAMPLE, back: 1 };
        unsafe {
            ctx.CopySubresourceRegion(&self.staging, 0, 0, 0, 0, src, 0, Some(&b));
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let mut h: u64 = 0xcbf29ce484222325;
            for row in 0..SAMPLE {
                let p = (m.pData as *const u8).add((row * m.RowPitch) as usize);
                let line = std::slice::from_raw_parts(p, (SAMPLE * 4) as usize);
                for &v in line {
                    h ^= v as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
            ctx.Unmap(&self.staging, 0);
            Ok(h)
        }
    }
}

#[derive(Default)]
struct Sec {
    dd_present: u32,
    dd_pointer_only: u32,
    dd_timeout: u32,
    dd_lost: u32,
    dd_invalid: u32,
    dd_other: u32,
    dd_protected: u32,
    dd_hash_changes: u32,
    wgc_frames: u32,
    wgc_hash_changes: u32,
    win_frames: u32,
    win_hash_changes: u32,
}

fn foreground() -> String {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let h = GetForegroundWindow();
        let mut buf = [0u16; 128];
        let n = GetWindowTextW(h, &mut buf);
        let mut r = windows::Win32::Foundation::RECT::default();
        let _ = GetWindowRect(h, &mut r);
        format!("«{}» ({},{})-({},{})", String::from_utf16_lossy(&buf[..n.max(0) as usize]), r.left, r.top, r.right, r.bottom)
    }
}

/// Окно впереди закрывает весь монитор.
fn fg_covers(m: windows::Win32::Foundation::RECT) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let mut r = windows::Win32::Foundation::RECT::default();
        GetWindowRect(GetForegroundWindow(), &mut r).is_ok() && r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(20);
    let monitor: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let g = gpu::open(monitor)?;
    let odesc = unsafe { g.output.GetDesc()? };
    // перед стартом: число — подождать столько секунд, текст — дождаться окна с таким
    // заголовком впереди (окно впереди захватывается отдельно, это должна быть игра)
    match args.get(3) {
        Some(a) if a.parse::<u64>().is_ok() => std::thread::sleep(Duration::from_secs(a.parse()?)),
        Some(title) => {
            println!("жду окно «{title}» впереди…");
            let t0 = Instant::now();
            let r = odesc.DesktopCoordinates;
            while !(foreground().contains(title.as_str()) && fg_covers(r)) {
                if t0.elapsed() > Duration::from_secs(1200) {
                    anyhow::bail!("окно «{title}» так и не появилось впереди");
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            // дать игре развернуться на весь экран
            std::thread::sleep(Duration::from_secs(5));
        }
        None => {}
    }
    unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };

    println!("монитор {} {:?}", g.monitor_name, odesc.DesktopCoordinates);
    let sampler = Sampler::new(&g.device)?;

    // Desktop Duplication
    let mut dup = Some(unsafe { g.output.DuplicateOutput(&g.device)? });

    // Windows.Graphics.Capture
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(odesc.Monitor)? };
    let size = item.Size()?;
    let dxgi: IDXGIDevice = g.device.cast()?;
    let rt_dev: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&rt_dev, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetIsCursorCaptureEnabled(false);
    session.StartCapture()?;
    println!("WGC: {}×{}", size.Width, size.Height);

    // WGC окна, которое сейчас впереди (игры)
    let fg = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    let win_item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(fg)? };
    let win_size = win_item.Size()?;
    let win_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&rt_dev, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, win_size)?;
    let win_session = win_pool.CreateCaptureSession(&win_item)?;
    let _ = win_session.SetIsBorderRequired(false);
    let _ = win_session.SetIsCursorCaptureEnabled(false);
    win_session.StartCapture()?;
    println!("WGC окна {}: {}×{}", foreground(), win_size.Width, win_size.Height);
    let mut win_last = 0u64;

    let start = Instant::now();
    let mut sec = Sec::default();
    let mut tick = Instant::now();
    let (mut dd_last, mut wgc_last) = (0u64, 0u64);
    while start.elapsed() < Duration::from_secs(secs) {
        if let Some(d) = &dup {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match unsafe { d.AcquireNextFrame(5, &mut info, &mut res) } {
                Ok(()) => {
                    if info.ProtectedContentMaskedOut.as_bool() {
                        sec.dd_protected += 1;
                    }
                    if info.LastPresentTime != 0 {
                        sec.dd_present += 1;
                        if let Some(tex) = res.and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                            let h = sampler.hash(&g.context, &tex)?;
                            if h != dd_last {
                                sec.dd_hash_changes += 1;
                                dd_last = h;
                            }
                        }
                    } else {
                        sec.dd_pointer_only += 1;
                    }
                    unsafe {
                        let _ = d.ReleaseFrame();
                    }
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => sec.dd_timeout += 1,
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    sec.dd_lost += 1;
                    dup = None;
                }
                Err(e) if e.code() == DXGI_ERROR_INVALID_CALL => sec.dd_invalid += 1,
                Err(e) => {
                    sec.dd_other += 1;
                    eprintln!("DD: {e}");
                }
            }
        } else {
            dup = unsafe { g.output.DuplicateOutput(&g.device) }.ok();
            std::thread::sleep(Duration::from_millis(5));
        }
        while let Ok(f) = pool.TryGetNextFrame() {
            sec.wgc_frames += 1;
            let access: IDirect3DDxgiInterfaceAccess = f.Surface()?.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let h = sampler.hash(&g.context, &tex)?;
            if h != wgc_last {
                sec.wgc_hash_changes += 1;
                wgc_last = h;
            }
        }
        while let Ok(f) = win_pool.TryGetNextFrame() {
            sec.win_frames += 1;
            let access: IDirect3DDxgiInterfaceAccess = f.Surface()?.cast()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let h = sampler.hash(&g.context, &tex)?;
            if h != win_last {
                sec.win_hash_changes += 1;
                win_last = h;
            }
        }
        if tick.elapsed() >= Duration::from_secs(1) {
            tick = Instant::now();
            println!(
                "{:>3} с | DD: картинка {:>3} (центр менялся {:>3}), только мышь {:>3}, ожиданий {:>3}, потерь {}, INVALID_CALL {}, прочих {}, защищено {} | WGC: кадров {:>3} (центр менялся {:>3}) | окно: {:>3} ({:>3}) | впереди {}",
                start.elapsed().as_secs(),
                sec.dd_present,
                sec.dd_hash_changes,
                sec.dd_pointer_only,
                sec.dd_timeout,
                sec.dd_lost,
                sec.dd_invalid,
                sec.dd_other,
                sec.dd_protected,
                sec.wgc_frames,
                sec.wgc_hash_changes,
                sec.win_frames,
                sec.win_hash_changes,
                foreground()
            );
            sec = Sec::default();
        }
    }
    win_session.Close()?;
    session.Close()?;
    pool.Close()?;
    Ok(())
}
