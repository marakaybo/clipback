//! Монтаж без интерфейса — проверить склейку на копии клипа.
//!
//!   cargo run --release --example montage -- <клип.mp4> <выход.mp4> 1.5-4 7-9.25 12-20 [--gains 1,0.5,2]

use anyhow::{bail, Context, Result};
use clipback_core::montage;
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        bail!("нужно: <клип> <выход> s-e [s-e …] [--gains g0,g1,…]");
    }
    let (src, dst) = (Path::new(&args[0]), Path::new(&args[1]));
    let mut ranges = Vec::new();
    let mut gains: Option<Vec<f32>> = None;
    let mut it = args[2..].iter();
    while let Some(a) = it.next() {
        if a == "--gains" {
            gains = Some(it.next().context("--gains без значений")?.split(',').map(|g| g.parse()).collect::<Result<_, _>>()?);
            continue;
        }
        let (s, e) = a.split_once('-').context("кусок пишется как 1.5-4")?;
        ranges.push((s.parse::<f64>()?, e.parse::<f64>()?));
    }
    let q = montage::source_quality(src)?;
    let t = std::time::Instant::now();
    let r = montage::montage(src, dst, &ranges, None, gains.as_deref(), &q, &mut |_| {}, &AtomicBool::new(false))?;
    let (times, end) = montage::frame_times(src)?;
    for p in montage::plan(&times, end, &ranges) {
        println!("кусок {:.4}–{:.4} с -> с {:.4} с, кадров {}", p.src_start as f64 / 1e7, p.src_end as f64 / 1e7, p.out_start as f64 / 1e7, p.frames);
    }
    println!("готово: {:.4} с, кусков {}, битрейт {:?}, за {:.1} с", r.duration, r.pieces, q.fixed_bitrate, t.elapsed().as_secs_f64());
    Ok(())
}
