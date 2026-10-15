//! BGRA -> NV12 (+ масштабирование) на видеокарте через D3D11 Video Processor.

use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

pub struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumr: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    input_view: Option<ID3D11VideoProcessorInputView>,
    out_w: u32,
    out_h: u32,
}

impl Converter {
    /// `src` — постоянная BGRA-текстура, куда копируется последний кадр экрана.
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, src: &ID3D11Texture2D, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        let sd = crate::capture::texture_desc(src);
        let mut c = Self::build(device, context, sd.Width, sd.Height, out_w, out_h, fps, false)?;
        c.input_view = Some(c.view_for(src, 0)?);
        Ok(c)
    }

    /// Для кадров видео (NV12 из декодера): вход — YUV BT.709, кадры подаются через `convert_texture`.
    pub fn for_video(device: &ID3D11Device, context: &ID3D11DeviceContext, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        Self::build(device, context, in_w, in_h, out_w, out_h, fps, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(device: &ID3D11Device, context: &ID3D11DeviceContext, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32, yuv_in: bool) -> Result<Self> {
        let sd = D3D11_TEXTURE2D_DESC { Width: in_w, Height: in_h, ..Default::default() };
        let vdev: ID3D11VideoDevice = device.cast().context("видеокарта не поддерживает D3D11 Video")?;
        let vctx: ID3D11VideoContext = context.cast()?;

        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
            InputWidth: sd.Width,
            InputHeight: sd.Height,
            OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumr = unsafe { vdev.CreateVideoProcessorEnumerator(&content)? };
        let vp = unsafe { vdev.CreateVideoProcessor(&enumr, 0)? };

        unsafe {
            // вход: RGB полного диапазона; выход: YUV BT.709, ограниченный диапазон (стандарт для видео)
            let input_cs = if yuv_in { (1 << 2) | (1 << 4) } else { 0 };
            vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: input_cs });
            vctx.VideoProcessorSetOutputColorSpace(&vp, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) });
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            let full_in = RECT { left: 0, top: 0, right: sd.Width as i32, bottom: sd.Height as i32 };
            let full_out = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&full_in));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&full_out));
            vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&full_out));
        }

        Ok(Self { vdev, vctx, enumr, vp, input_view: None, out_w, out_h })
    }

    fn view_for(&self, src: &ID3D11Texture2D, slice: u32) -> Result<ID3D11VideoProcessorInputView> {
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice } },
        };
        let mut v = None;
        unsafe { self.vdev.CreateVideoProcessorInputView(src, &self.enumr, &in_desc, Some(&mut v))? };
        v.context("не удалось подключить кадр к преобразователю")
    }

    /// Преобразовать кадр из любой текстуры (например, слой `slice` текстуры декодера).
    pub fn convert_texture(&self, src: &ID3D11Texture2D, slice: u32, target: &ID3D11VideoProcessorOutputView) -> Result<()> {
        let view = self.view_for(src, slice)?;
        self.blt(&view, target)
    }

    /// Создать NV12-текстуру нужного размера и вид для записи в неё.
    pub fn make_target(&self, device: &ID3D11Device) -> Result<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)> {
        // сначала пробуем с флагом для кодировщика, если драйвер не принимает — без него
        let tex = create_texture(device, self.out_w, self.out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_VIDEO_ENCODER.0 as u32)
            .or_else(|_| create_texture(device, self.out_w, self.out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32))?;
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        unsafe { self.vdev.CreateVideoProcessorOutputView(&tex, &self.enumr, &out_desc, Some(&mut view))? };
        Ok((tex, view.unwrap()))
    }

    pub fn convert(&self, target: &ID3D11VideoProcessorOutputView) -> Result<()> {
        let view = self.input_view.clone().context("нет входного кадра")?;
        self.blt(&view, target)
    }

    fn blt(&self, input: &ID3D11VideoProcessorInputView, target: &ID3D11VideoProcessorOutputView) -> Result<()> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.vp, target, 0, &streams) };
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        r.context("ошибка преобразования кадра")?;
        Ok(())
    }
}

pub fn create_texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT, bind: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    Ok(tex.unwrap())
}

/// Текстура, в которую копируется кадр экрана (вход преобразователя).
pub fn create_frame_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    create_texture(device, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32)
}
