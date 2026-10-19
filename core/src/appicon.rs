//! Значок приложения для микшера: PNG 32×32 по пути к exe. Процесс при этом не открывается —
//! значок берётся из файла. У приложений из Microsoft Store (WhatsApp) в exe значка может не
//! быть — тогда берём логотип пакета из `AppxManifest.xml`. Запоминается по пути к exe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::{ExtractIconExW, SHDefExtractIconW};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

pub const SIZE: u32 = 32;

/// PNG значка exe (None — значка нет, показываем общий). Запоминается, и неудача тоже.
pub fn icon_png(exe_path: &str) -> Option<Vec<u8>> {
    static CACHE: Mutex<Option<HashMap<String, Option<Vec<u8>>>>> = Mutex::new(None);
    let key = exe_path.to_lowercase();
    if let Some(v) = CACHE.lock().ok().and_then(|c| c.as_ref().and_then(|m| m.get(&key).cloned())) {
        return v;
    }
    let png = exe_icon(exe_path).or_else(|| package_logo(Path::new(exe_path)));
    if let Ok(mut c) = CACHE.lock() {
        c.get_or_insert_with(HashMap::new).insert(key, png.clone());
    }
    png
}

/// Значок из ресурсов exe нужного размера.
fn exe_icon(path: &str) -> Option<Vec<u8>> {
    let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        // в exe нет значков — Windows нарисовала бы общий значок программы
        if ExtractIconExW(PCWSTR(w.as_ptr()), -1, None, None, 0) == 0 {
            return None;
        }
        let mut icon = HICON::default();
        SHDefExtractIconW(PCWSTR(w.as_ptr()), 0, 0, Some(&mut icon), None, SIZE).ok().ok()?;
        if icon.is_invalid() {
            return None;
        }
        let rgba = icon_rgba(icon);
        let _ = DestroyIcon(icon);
        let (rgba, w, h) = rgba?;
        encode_png(&rgba, w, h)
    }
}

/// Пиксели значка в RGBA (без премультипликации, как в самом значке).
unsafe fn icon_rgba(icon: HICON) -> Option<(Vec<u8>, u32, u32)> {
    let mut info = ICONINFO::default();
    GetIconInfo(icon, &mut info).ok()?;
    let dc = CreateCompatibleDC(None);
    let read = |bmp: HBITMAP| -> Option<(Vec<u8>, u32, u32)> {
        if bmp.is_invalid() {
            return None;
        }
        let mut bm = BITMAP::default();
        if GetObjectW(bmp.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth.max(0) as u32, bm.bmHeight.max(0) as u32);
        if w == 0 || h == 0 || w > 256 || h > 512 {
            return None;
        }
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        (GetDIBits(dc, bmp, 0, h, Some(px.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS) == h as i32).then_some((px, w, h))
    };
    let color = read(info.hbmColor);
    let mask = read(info.hbmMask);
    let _ = DeleteDC(dc);
    if !info.hbmColor.is_invalid() {
        let _ = DeleteObject(info.hbmColor.into());
    }
    if !info.hbmMask.is_invalid() {
        let _ = DeleteObject(info.hbmMask.into());
    }
    let (mut px, w, h) = color?;
    let has_alpha = px.chunks_exact(4).any(|p| p[3] != 0);
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        p.swap(0, 2); // BGRA -> RGBA
        if !has_alpha {
            // старый значок без альфы: прозрачность — из маски (белое — прозрачно)
            let opaque = mask.as_ref().filter(|(_, mw, _)| *mw == w).map(|(m, _, _)| m[i * 4] == 0).unwrap_or(true);
            p[3] = if opaque { 255 } else { 0 };
        }
    }
    Some((px, w, h))
}

fn encode_png(rgba: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().ok()?.write_image_data(rgba).ok()?;
    Some(out)
}

// ---------- логотип пакета (Microsoft Store) ----------

/// Папка пакета: где рядом с exe (или выше) лежит `AppxManifest.xml`.
fn package_dir(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().skip(1).take(4).find(|d| d.join("AppxManifest.xml").is_file()).map(Path::to_path_buf)
}

/// Имя приложения из Microsoft Store («WhatsApp», а не «WhatsApp.Root» из exe).
pub fn package_name(exe_path: &str) -> Option<String> {
    if !exe_path.to_lowercase().contains(r"\windowsapps\") {
        return None;
    }
    let xml = std::fs::read_to_string(package_dir(Path::new(exe_path))?.join("AppxManifest.xml")).ok()?;
    manifest_name(&xml)
}

/// `DisplayName` плитки приложения, иначе пакета; ссылки на ресурсы («ms-resource:») не годятся.
pub fn manifest_name(xml: &str) -> Option<String> {
    let attr = xml.find("DisplayName=\"").and_then(|i| xml[i + 13..].split('"').next());
    let tag = xml.find("<DisplayName>").and_then(|i| xml[i + 13..].split('<').next());
    [attr, tag].into_iter().flatten().map(str::trim).find(|n| !n.is_empty() && !n.starts_with("ms-resource:")).map(str::to_string)
}

/// Логотип из манифеста: `Square44x44Logo` (значок в панели задач), иначе `Logo`.
pub fn manifest_logo(xml: &str) -> Option<String> {
    let attr = |name: &str| {
        let pat = format!("{name}=\"");
        let i = xml.find(&pat)? + pat.len();
        Some(xml[i..].split('"').next()?.to_string())
    };
    let tag = || {
        let i = xml.find("<Logo>")? + 6;
        Some(xml[i..].split('<').next()?.trim().to_string())
    };
    attr("Square44x44Logo").or_else(|| attr("Square150x150Logo")).or_else(tag).filter(|s| !s.is_empty())
}

/// Из вариантов логотипа («Logo.targetsize-32_altform-unplated.png», «Logo.scale-200.png»)
/// — лучший для 32 px: без подложки, не контрастный, наименьший не меньше 32.
pub fn pick_variant<'a>(names: &[(&'a str, u32)]) -> Option<&'a str> {
    let score = |n: &str, w: u32| {
        let l = n.to_lowercase();
        let bad = l.contains("contrast-") as u32 * 4;
        let plated = !l.contains("unplated") as u32;
        let small = (w < SIZE) as u32 * 2;
        (bad + small + plated, if w >= SIZE { w } else { u32::MAX - w })
    };
    names.iter().min_by_key(|(n, w)| score(n, *w)).map(|(n, _)| *n)
}

fn package_logo(exe: &Path) -> Option<Vec<u8>> {
    let dir = package_dir(exe)?;
    let xml = std::fs::read_to_string(dir.join("AppxManifest.xml")).ok()?;
    let logo = PathBuf::from(manifest_logo(&xml)?.replace('/', "\\"));
    let folder = dir.join(logo.parent().unwrap_or(Path::new("")));
    let stem = logo.file_stem()?.to_string_lossy().to_lowercase();
    let mut found: Vec<(String, u32)> = Vec::new();
    for e in std::fs::read_dir(&folder).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let l = name.to_lowercase();
        if l.ends_with(".png") && (l == format!("{stem}.png") || l.starts_with(&format!("{stem}."))) {
            if let Some(w) = png_width(&e.path()) {
                found.push((name, w));
            }
        }
    }
    let names: Vec<(&str, u32)> = found.iter().map(|(n, w)| (n.as_str(), *w)).collect();
    let best = pick_variant(&names)?;
    let (rgba, w, h) = decode_png(&std::fs::read(folder.join(best)).ok()?)?;
    let small = resize(&rgba, w, h, SIZE);
    encode_png(&small, SIZE, SIZE)
}

fn png_width(p: &Path) -> Option<u32> {
    let f = std::fs::File::open(p).ok()?;
    let r = png::Decoder::new(std::io::BufReader::new(f)).read_info().ok()?;
    Some(r.info().width)
}

fn decode_png(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut d = png::Decoder::new(std::io::Cursor::new(data));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info().ok()?;
    let (w, h) = (r.info().width, r.info().height);
    if w == 0 || h == 0 || w > 2048 || h > 2048 {
        return None;
    }
    let mut buf = vec![0; r.output_buffer_size()];
    let f = r.next_frame(&mut buf).ok()?;
    let src = &buf[..f.buffer_size()];
    let rgba: Vec<u8> = match f.color_type {
        png::ColorType::Rgba => src.to_vec(),
        png::ColorType::Rgb => src.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => src.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => src.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        _ => return None,
    };
    Some((rgba, w, h))
}

/// Вписать в квадрат `size` по центру, усредняя пиксели (с учётом прозрачности).
pub fn resize(rgba: &[u8], w: u32, h: u32, size: u32) -> Vec<u8> {
    let scale = w.max(h) as f32 / size as f32;
    let (nw, nh) = (((w as f32 / scale).round() as u32).clamp(1, size), ((h as f32 / scale).round() as u32).clamp(1, size));
    let (ox, oy) = ((size - nw) / 2, (size - nh) / 2);
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let (x0, x1) = ((x as f32 * scale) as u32, (((x + 1) as f32 * scale).ceil() as u32).min(w).max(x as u32 + 1));
            let (y0, y1) = ((y as f32 * scale) as u32, (((y + 1) as f32 * scale).ceil() as u32).min(h).max(y as u32 + 1));
            let mut acc = [0f32; 4];
            let mut n = 0f32;
            for sy in y0..y1.min(h) {
                for sx in x0..x1.min(w) {
                    let i = ((sy * w + sx) * 4) as usize;
                    let a = rgba[i + 3] as f32 / 255.0;
                    acc[0] += rgba[i] as f32 * a;
                    acc[1] += rgba[i + 1] as f32 * a;
                    acc[2] += rgba[i + 2] as f32 * a;
                    acc[3] += a;
                    n += 1.0;
                }
            }
            let o = (((y + oy) * size + x + ox) * 4) as usize;
            if acc[3] > 0.0 {
                out[o] = (acc[0] / acc[3]).round() as u8;
                out[o + 1] = (acc[1] / acc[3]).round() as u8;
                out[o + 2] = (acc[2] / acc[3]).round() as u8;
            }
            out[o + 3] = (acc[3] / n.max(1.0) * 255.0).round() as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_logo_prefers_taskbar_icon() {
        let xml = r#"<Properties><Logo>Assets\StoreLogo.png</Logo></Properties>
            <uap:VisualElements DisplayName="WhatsApp" Square150x150Logo="Assets\Big.png" Square44x44Logo="Assets\Small.png" />"#;
        assert_eq!(manifest_logo(xml).as_deref(), Some("Assets\\Small.png"));
        assert_eq!(manifest_logo("<Logo>Assets\\StoreLogo.png</Logo>").as_deref(), Some("Assets\\StoreLogo.png"));
        assert_eq!(manifest_logo("<Package/>"), None);
    }

    #[test]
    fn manifest_display_name() {
        let xml = r#"<Properties><DisplayName>ms-resource:AppName</DisplayName></Properties>
            <uap:VisualElements DisplayName="WhatsApp" Square44x44Logo="a.png" />"#;
        assert_eq!(manifest_name(xml).as_deref(), Some("WhatsApp"));
        let xml = r#"<DisplayName>Spotify</DisplayName><uap:VisualElements DisplayName="ms-resource:Name" />"#;
        assert_eq!(manifest_name(xml).as_deref(), Some("Spotify"));
        assert_eq!(manifest_name("<Package/>"), None);
    }

    /// Логотип пакета Store без значка в exe: `ICON_EXE=<путь к exe из WindowsApps> cargo test --lib package_logo_of -- --ignored`.
    #[test]
    #[ignore]
    fn package_logo_of_installed_app() {
        let exe = std::env::var("ICON_EXE").unwrap();
        let png = package_logo(Path::new(&exe)).expect("логотип пакета");
        let (_, w, h) = decode_png(&png).unwrap();
        assert_eq!((w, h), (SIZE, SIZE));
        println!("{exe}: имя {:?}, логотип {} байт", package_name(&exe), png.len());
        if let Ok(out) = std::env::var("ICON_OUT") {
            std::fs::write(out, png).unwrap();
        }
    }

    #[test]
    fn best_logo_variant_for_32px() {
        let v = [
            ("Small.scale-100.png", 44),
            ("Small.targetsize-32.png", 32),
            ("Small.targetsize-32_altform-unplated.png", 32),
            ("Small.targetsize-48_altform-unplated.png", 48),
            ("Small.targetsize-16_altform-unplated.png", 16),
            ("Small.targetsize-32_contrast-black.png", 32),
        ];
        assert_eq!(pick_variant(&v), Some("Small.targetsize-32_altform-unplated.png"));
        assert_eq!(pick_variant(&[("A.png", 16), ("A.scale-200.png", 88)]), Some("A.scale-200.png"));
        assert_eq!(pick_variant(&[]), None);
    }

    #[test]
    fn resize_keeps_square_and_alpha() {
        // 64×32 красный — вписывается 32×16 по центру, сверху и снизу прозрачно
        let src: Vec<u8> = (0..64 * 32).flat_map(|_| [255, 0, 0, 255]).collect();
        let out = resize(&src, 64, 32, 32);
        assert_eq!(out.len(), 32 * 32 * 4);
        assert_eq!(&out[(16 * 32 + 16) * 4..][..4], &[255, 0, 0, 255]);
        assert_eq!(out[3], 0);
    }

    #[test]
    fn explorer_icon_is_cached() {
        let png = icon_png(r"C:\Windows\explorer.exe").expect("значок проводника");
        let (_, w, h) = decode_png(&png).unwrap();
        assert_eq!((w, h), (SIZE, SIZE));
        // повторно — из памяти, тот же результат
        assert_eq!(icon_png(r"C:\WINDOWS\explorer.exe"), Some(png));
        assert_eq!(icon_png(r"C:\нет\такого.exe"), None);
    }
}
