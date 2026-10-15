//! Ядро Clipback: захват экрана, аппаратное кодирование, буфер последних минут
//! и сохранение клипов без перекодирования.
//!
//! Конвейер (всё на видеокарте, кадр не уходит в процессор):
//!   DXGI Desktop Duplication (BGRA) -> D3D11 Video Processor (NV12)
//!   -> Media Foundation H.264 (NVENC/AMF/QSV) -> кольцевой буфер в ОЗУ -> MP4

pub mod aac;
pub mod audio;
pub mod capture;
pub mod convert;
pub mod encoder;
pub mod gpu;
pub mod hotkey;
pub mod live;
pub mod media;
pub mod mux;
pub mod recorder;
pub mod mixer;
pub mod ring;
pub mod shot;
pub mod util;

pub use recorder::{Command, Config, Event, Recorder};
