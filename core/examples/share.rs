//! Копия с одной дорожкой и отрезок без интерфейса — проверить звук на копии клипа.
//!
//!   cargo run --release --example share -- one <клип.mp4> <выход.mp4> [--gains 1,1,0.5]
//!   cargo run --release --example share -- trim <клип.mp4> <выход.mp4> <начало> <конец> [--gains …]

use anyhow::{bail, Context, Result};
use clipback_core::{media, mux};
use std::path::Path;

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let gains: Option<Vec<f32>> = match args.iter().position(|a| a == "--gains") {
        Some(i) => {
            let g = args.get(i + 1).context("--gains без значений")?.split(',').map(|g| g.parse()).collect::<Result<_, _>>()?;
            args.drain(i..i + 2);
            Some(g)
        }
        None => None,
    };
    if args.len() < 3 {
        bail!("нужно: one|trim <клип> <выход> [начало конец] [--gains g0,g1,…]");
    }
    let (src, dst) = (Path::new(&args[1]), Path::new(&args[2]));
    let t = std::time::Instant::now();
    let mut steps = 0;
    let mut progress = |_: f64| steps += 1;
    match args[0].as_str() {
        "one" => mux::one_track(src, dst, gains.as_deref(), &mut progress, &media::NEVER)?,
        "trim" => {
            let (s, e) = (args[3].parse()?, args[4].parse()?);
            mux::trim_tracks_with(src, dst, s, e, None, gains.as_deref(), &mut progress, &media::NEVER)?;
        }
        _ => bail!("неизвестная команда"),
    }
    println!("готово за {:.2} с, событий хода работы: {steps}", t.elapsed().as_secs_f64());
    Ok(())
}
