//! Скриншот текущего кадра: копия текстуры с видеокарты в память и PNG.

use crate::capture::texture_desc;
use crate::gpu::Gpu;
use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

pub fn grab_png(g: &Gpu, frame: &ID3D11Texture2D) -> Result<Vec<u8>> {
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
    encode_png(&rgba, w as u32, h as u32)
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

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_valid_png() {
        let png = super::encode_png(&[255, 0, 0, 255, 0, 255, 0, 255], 2, 1).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 1);
    }
}
