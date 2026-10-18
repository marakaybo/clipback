//! Скриншот текущего кадра: копия текстуры с видеокарты в память, затем PNG или JPEG.
//! Поток захвата только забирает пиксели, а сжатие делает тот, кто попросил снимок, —
//! так кодирование картинки не задерживает запись.

use crate::capture::texture_desc;
use crate::gpu::Gpu;
use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

/// Несжатый кадр, RGBA.
pub struct RawImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub fn grab(g: &Gpu, frame: &ID3D11Texture2D) -> Result<RawImage> {
    let d = texture_desc(frame);
    let desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..d
    };
    let mut staging = None;
    unsafe { g.device.CreateTexture2D(&desc, None, Some(&mut staging))? };
    let staging = staging.context("не удалось создать текстуру для скриншота")?;

    let (w, h) = (d.Width as usize, d.Height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    unsafe {
        g.context.CopyResource(&staging, frame);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let pitch = m.RowPitch as usize;
        let src = std::slice::from_raw_parts(m.pData as *const u8, pitch * h);
        for y in 0..h {
            let row = &src[y * pitch..y * pitch + w * 4];
            let out = &mut rgba[y * w * 4..(y + 1) * w * 4];
            for (o, i) in out.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                // BGRA -> RGBA, альфа всегда непрозрачная
                o[0] = i[2];
                o[1] = i[1];
                o[2] = i[0];
                o[3] = 255;
            }
        }
        g.context.Unmap(&staging, 0);
    }
    Ok(RawImage { rgba, width: w as u32, height: h as u32 })
}

/// Сжать кадр: `jpeg` — JPEG (качество 92, в 5–10 раз легче), иначе PNG без потерь.
/// Возвращает данные и расширение файла.
pub fn encode(img: &RawImage, jpeg: bool) -> Result<(Vec<u8>, &'static str)> {
    if jpeg {
        Ok((encode_jpeg(&img.rgba, img.width, img.height, 92)?, "jpg"))
    } else {
        Ok((encode_png(&img.rgba, img.width, img.height)?, "png"))
    }
}

pub fn encode_jpeg(rgba: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>> {
    let (w16, h16) = (u16::try_from(w).context("слишком широкая картинка")?, u16::try_from(h).context("слишком высокая картинка")?);
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality).encode(rgba, w16, h16, jpeg_encoder::ColorType::Rgba)?;
    Ok(out)
}

pub fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        // быстрое сжатие: скриншот нужен сразу, а не через секунду
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header()?;
        wr.write_image_data(rgba)?;
    }
    Ok(out)
}

/// Формат картинки по первым байтам: "png", "jpg" или None.
pub fn image_kind(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_valid_png() {
        let png = encode_png(&[255, 0, 0, 255, 0, 255, 0, 255], 2, 1).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 1);
        assert_eq!(image_kind(&png), Some("png"));
    }

    #[test]
    fn jpeg_is_much_smaller_on_photo_like_content() {
        // плавный градиент с шумом — как кадр из игры
        let (w, h) = (320u32, 180u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        let mut seed = 7u32;
        for y in 0..h {
            for x in 0..w {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let n = (seed >> 24) as u8 / 8;
                rgba.extend_from_slice(&[((x * 255 / w) as u8).wrapping_add(n), (y * 255 / h) as u8, 128 + n, 255]);
            }
        }
        let img = RawImage { rgba, width: w, height: h };
        let (jpg, ext) = encode(&img, true).unwrap();
        let (png, _) = encode(&img, false).unwrap();
        assert_eq!(ext, "jpg");
        assert_eq!(image_kind(&jpg), Some("jpg"));
        assert!(jpg.len() * 3 < png.len(), "jpg {} png {}", jpg.len(), png.len());
        assert_eq!(image_kind(b"GIF89a"), None);
    }
}
