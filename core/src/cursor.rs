//! Курсор мыши в записи. Desktop Duplication отдаёт картинку экрана без курсора, а его
//! положение и форму — отдельно. Здесь форма переводится в текстуры и рисуется на видеокарте
//! поверх копии кадра перед кодированием.
//!
//! Курсоры бывают трёх видов: цветные с прозрачностью, «маскированные» и чёрно-белые.
//! У двух последних часть пикселей не закрашивается, а инвертирует то, что под ними
//! (так устроен текстовый курсор «I»). Это рисуется вторым проходом со смешиванием
//! «1 − цвет под курсором».

use anyhow::{Context, Result};
use windows::core::{s, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

pub const SHAPE_MONOCHROME: u32 = 1;
pub const SHAPE_COLOR: u32 = 2;
pub const SHAPE_MASKED_COLOR: u32 = 4;

/// Форма курсора в том виде, как её отдаёт Desktop Duplication.
#[derive(Clone)]
pub struct PointerShape {
    pub kind: u32,
    pub width: u32,
    /// Для чёрно-белого — удвоенная высота (маска AND, затем XOR).
    pub height: u32,
    pub pitch: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub data: Vec<u8>,
}

/// Где курсор и как выглядит.
#[derive(Clone, Default)]
pub struct Pointer {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub shape: Option<PointerShape>,
    /// Растёт при каждой смене формы.
    pub shape_version: u64,
}

/// Курсор в виде двух картинок BGRA: обычная (умноженная альфа) и маска инверсии.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub color: Vec<u8>,
    pub invert: Vec<u8>,
    pub has_invert: bool,
}

pub fn decode(s: &PointerShape) -> Decoded {
    let w = s.width as usize;
    let h = if s.kind == SHAPE_MONOCHROME { s.height as usize / 2 } else { s.height as usize };
    let mut color = vec![0u8; w * h * 4];
    let mut invert = vec![0u8; w * h * 4];
    let mut has_invert = false;
    let pitch = s.pitch as usize;
    let put = |buf: &mut Vec<u8>, i: usize, b: u8, g: u8, r: u8, a: u8| {
        buf[i * 4..i * 4 + 4].copy_from_slice(&[b, g, r, a]);
    };
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            match s.kind {
                SHAPE_MONOCHROME => {
                    let bit = |row: usize| s.data.get(row * pitch + x / 8).is_some_and(|v| v & (0x80 >> (x % 8)) != 0);
                    let (and, xor) = (bit(y), bit(y + h));
                    match (and, xor) {
                        (false, false) => put(&mut color, i, 0, 0, 0, 255),
                        (false, true) => put(&mut color, i, 255, 255, 255, 255),
                        (true, true) => {
                            put(&mut invert, i, 255, 255, 255, 255);
                            has_invert = true;
                        }
                        (true, false) => {} // прозрачно
                    }
                }
                _ => {
                    let o = y * pitch + x * 4;
                    let Some(p) = s.data.get(o..o + 4) else { continue };
                    let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
                    if s.kind == SHAPE_MASKED_COLOR {
                        if a == 0 {
                            put(&mut color, i, b, g, r, 255);
                        } else if (b, g, r) != (0, 0, 0) {
                            // XOR с цветом — почти всегда белым: рисуем как инверсию
                            put(&mut invert, i, 255, 255, 255, 255);
                            has_invert = true;
                        }
                    } else {
                        // цветной: альфа обычная — умножаем цвет на неё
                        let m = |c: u8| (c as u32 * a as u32 / 255) as u8;
                        put(&mut color, i, m(b), m(g), m(r), a);
                    }
                }
            }
        }
    }
    Decoded { width: w as u32, height: h as u32, color, invert, has_invert }
}

const SHADER: &str = r#"
cbuffer Rect : register(b0) { float4 rect; } // x0, y0, x1, y1 в NDC
Texture2D tex : register(t0);
SamplerState smp : register(s0);
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    float2 t = float2(id & 1, id >> 1);
    V o;
    o.pos = float4(lerp(rect.x, rect.z, t.x), lerp(rect.y, rect.w, t.y), 0, 1);
    o.uv = t;
    return o;
}
float4 ps(V i) : SV_Target { return tex.Sample(smp, i.uv); }
"#;

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut blob = None;
    let mut errors = None;
    let r = unsafe { D3DCompile(SHADER.as_ptr() as _, SHADER.len(), None, None, None, entry, target, 0, 0, &mut blob, Some(&mut errors)) };
    if let Err(e) = r {
        let msg = errors
            .map(|b| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned() })
            .unwrap_or_default();
        anyhow::bail!("шейдер курсора: {e} {msg}");
    }
    blob.context("пустой шейдер")
}

fn bytes(b: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

struct ShapeTex {
    color: ID3D11ShaderResourceView,
    invert: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
}

pub struct CursorPainter {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    blend_normal: ID3D11BlendState,
    blend_invert: ID3D11BlendState,
    cb: ID3D11Buffer,
    shape: Option<ShapeTex>,
    version: u64,
}

impl CursorPainter {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vsb = compile(s!("vs"), s!("vs_4_0"))?;
        let psb = compile(s!("ps"), s!("ps_4_0"))?;
        unsafe {
            let mut vs = None;
            device.CreateVertexShader(bytes(&vsb), None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(bytes(&psb), None, Some(&mut ps))?;
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;
            let blend = |src: D3D11_BLEND, dst: D3D11_BLEND| -> Result<ID3D11BlendState> {
                let mut d = D3D11_BLEND_DESC::default();
                d.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                    BlendEnable: true.into(),
                    SrcBlend: src,
                    DestBlend: dst,
                    BlendOp: D3D11_BLEND_OP_ADD,
                    SrcBlendAlpha: D3D11_BLEND_ZERO,
                    DestBlendAlpha: D3D11_BLEND_ONE,
                    BlendOpAlpha: D3D11_BLEND_OP_ADD,
                    RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
                };
                let mut s = None;
                device.CreateBlendState(&d, Some(&mut s))?;
                s.context("blend")
            };
            // обычный: цвет с умноженной альфой; инверсия: результат = 1 − под курсором
            let blend_normal = blend(D3D11_BLEND_ONE, D3D11_BLEND_INV_SRC_ALPHA)?;
            let blend_invert = blend(D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_ALPHA)?;
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut cb = None;
            device.CreateBuffer(&bd, None, Some(&mut cb))?;
            Ok(Self {
                vs: vs.context("vs")?,
                ps: ps.context("ps")?,
                sampler: sampler.context("sampler")?,
                blend_normal,
                blend_invert,
                cb: cb.context("cb")?,
                shape: None,
                version: 0,
            })
        }
    }

    fn texture(device: &ID3D11Device, w: u32, h: u32, px: &[u8]) -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr() as _, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
        unsafe {
            let mut t = None;
            device.CreateTexture2D(&desc, Some(&init), Some(&mut t))?;
            let t = t.context("текстура курсора")?;
            let mut v = None;
            device.CreateShaderResourceView(&t, None, Some(&mut v))?;
            v.context("вид текстуры курсора")
        }
    }

    /// Нарисовать курсор на `target` (кадр размером `tw`×`th`).
    pub fn draw(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, target: &ID3D11RenderTargetView, tw: u32, th: u32, p: &Pointer) -> Result<()> {
        if !p.visible {
            return Ok(());
        }
        let Some(shape) = &p.shape else { return Ok(()) };
        if self.shape.is_none() || self.version != p.shape_version {
            let d = decode(shape);
            if d.width == 0 || d.height == 0 {
                return Ok(());
            }
            self.shape = Some(ShapeTex {
                color: Self::texture(device, d.width, d.height, &d.color)?,
                invert: if d.has_invert { Some(Self::texture(device, d.width, d.height, &d.invert)?) } else { None },
                width: d.width,
                height: d.height,
            });
            self.version = p.shape_version;
        }
        let s = self.shape.as_ref().unwrap();
        // Desktop Duplication даёт сразу левый верхний угол картинки курсора (горячая точка уже учтена)
        let (x0, y0) = (p.x as f32, p.y as f32);
        let (x1, y1) = (x0 + s.width as f32, y0 + s.height as f32);
        let ndc = |x: f32, y: f32| (x / tw as f32 * 2.0 - 1.0, 1.0 - y / th as f32 * 2.0);
        let (a, b) = ndc(x0, y0);
        let (c, d) = ndc(x1, y1);
        let rect = [a, b, c, d];
        unsafe {
            ctx.UpdateSubresource(&self.cb, 0, None, rect.as_ptr() as _, 0, 0);
            let vp = D3D11_VIEWPORT { Width: tw as f32, Height: th as f32, MaxDepth: 1.0, ..Default::default() };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(s.color.clone())]));
            ctx.OMSetBlendState(&self.blend_normal, None, 0xffff_ffff);
            ctx.Draw(4, 0);
            if let Some(inv) = &s.invert {
                ctx.PSSetShaderResources(0, Some(&[Some(inv.clone())]));
                ctx.OMSetBlendState(&self.blend_invert, None, 0xffff_ffff);
                ctx.Draw(4, 0);
            }
            // чужое состояние конвейера не оставляем
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(None, None);
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_cursor_has_black_white_and_invert() {
        // 8×2: строки AND, затем строки XOR (по байту на строку)
        let and = [0b0000_1111u8, 0b0000_1111];
        let xor = [0b0011_0011u8, 0b0011_0011];
        let s = PointerShape { kind: SHAPE_MONOCHROME, width: 8, height: 4, pitch: 1, hot_x: 0, hot_y: 0, data: [and, xor].concat() };
        let d = decode(&s);
        assert_eq!((d.width, d.height), (8, 2));
        let px = |buf: &Vec<u8>, x: usize| buf[x * 4..x * 4 + 4].to_vec();
        assert_eq!(px(&d.color, 0), vec![0, 0, 0, 255]); // AND 0, XOR 0 — чёрный
        assert_eq!(px(&d.color, 2), vec![255, 255, 255, 255]); // AND 0, XOR 1 — белый
        assert_eq!(px(&d.color, 4), vec![0, 0, 0, 0]); // AND 1, XOR 0 — прозрачно
        assert_eq!(px(&d.invert, 6), vec![255, 255, 255, 255]); // AND 1, XOR 1 — инверсия
        assert!(d.has_invert);
    }

    #[test]
    fn color_cursor_is_premultiplied() {
        let s = PointerShape { kind: SHAPE_COLOR, width: 1, height: 1, pitch: 4, hot_x: 0, hot_y: 0, data: vec![200, 100, 50, 128] };
        let d = decode(&s);
        assert_eq!(d.color, vec![100, 50, 25, 128]);
        assert!(!d.has_invert);
    }
}
