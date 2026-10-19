//! Аудиосессии этого компьютера: чей процесс, к какому приложению отнесён, есть ли значок.
//! Ни один процесс не открывается. `cargo run --release --example mixer_owners`
use clipback_core::{proctree, volume};

fn main() -> anyhow::Result<()> {
    let procs = proctree::snapshot();
    let exe = |pid: u32| procs.get(&pid).map(|p| p.exe.clone()).unwrap_or_else(|| "?".into());
    println!("{:<7} {:<26} -> {:<7} {:<26}", "pid", "процесс сессии", "pid", "владелец (ключ)");
    for (pid, owner, key) in volume::session_owners()? {
        println!("{pid:<7} {:<26} -> {owner:<7} {key:<26}", if pid == 0 { "(системные звуки)".into() } else { exe(pid) });
    }
    println!();
    for a in volume::mixer_state()?.apps {
        let icon = if a.key == volume::SYSTEM_KEY { "системный".to_string() } else {
            match volume::app_icon(&a.key) {
                Some(png) => format!("PNG {} байт", png.len()),
                None => "общий".into(),
            }
        };
        println!("{:<28} {:<26} {:>4.0}%  значок: {icon}", a.name, a.key, a.volume * 100.0);
    }
    // значки по путям из командной строки (например, WhatsApp, когда он закрыт)
    for path in std::env::args().skip(1) {
        let icon = clipback_core::appicon::icon_png(&path);
        println!("{path}: {}", icon.as_ref().map_or("общий значок".into(), |p| format!("PNG {} байт", p.len())));
        if let (Some(png), Ok(dir)) = (icon, std::env::var("ICON_DIR")) {
            let name = std::path::Path::new(&path).file_stem().unwrap().to_string_lossy().to_string();
            std::fs::write(std::path::Path::new(&dir).join(format!("{name}.png")), png)?;
        }
    }
    Ok(())
}
