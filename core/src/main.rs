//! Консольный запуск ядра: пишет буфер и сохраняет клип по горячей клавише.
//! Позже сюда подключится оверлей, а пока так удобно проверять запись.

use anyhow::{bail, Context, Result};
use clipback_core::{gpu, hotkey, Config, Event, Recorder};
use std::io::BufRead;
use std::path::PathBuf;

const HELP: &str = "\
Clipback — запись последних минут

  clipback [параметры]

  --minutes N      длина буфера в минутах (по умолчанию 5)
  --fps N          кадров в секунду (по умолчанию 60)
  --bitrate N      битрейт в Мбит/с (по умолчанию 20)
  --monitor N      номер монитора (список: --list)
  --max-height N   уменьшить картинку до этой высоты, например 1080
  --mic            писать и микрофон (отдельной дорожкой)
  --mic-device ID  какой микрофон (id из настроек)
  --no-sound       без звука
  --hotkey K       клавиша сохранения (по умолчанию Alt+F10)
  --out ПАПКА      куда сохранять (по умолчанию Видео\\Clipback)
  --save-after N   сохранить через N секунд и выйти (для проверки)
  --list           показать мониторы

В окне консоли: Enter — сохранить, q + Enter — выйти.";

struct Args {
    cfg: Config,
    hotkey: String,
    save_after: Option<u64>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut cfg = Config::default();
    let mut hotkey = "Alt+F10".to_string();
    let mut save_after = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().with_context(|| format!("после {a} нужно значение"));
        match a.as_str() {
            "--minutes" => cfg.buffer_secs = (val()?.parse::<f64>()? * 60.0) as u32,
            "--fps" => cfg.fps = val()?.parse()?,
            "--bitrate" => cfg.bitrate_mbps = val()?.parse()?,
            "--monitor" => cfg.monitor = val()?.parse()?,
            "--max-height" => cfg.max_height = Some(val()?.parse()?),
            "--hotkey" => hotkey = val()?,
            "--out" => cfg.out_dir = PathBuf::from(val()?),
            "--save-after" => save_after = Some(val()?.parse()?),
            "--mic" => cfg.audio.mic = true,
            "--mic-device" => {
                cfg.audio.mic = true;
                cfg.audio.mic_device = Some(val()?);
            }
            "--no-sound" => {
                cfg.audio.system = false;
                cfg.audio.mic = false;
            }
            "--list" => {
                for (i, name, w, h) in gpu::list_monitors()? {
                    println!("  {i}: {name}  {w}×{h}");
                }
                return Ok(None);
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            other => bail!("неизвестный параметр {other}\n\n{HELP}"),
        }
    }
    Ok(Some(Args { cfg, hotkey, save_after }))
}

fn mb(b: u64) -> String {
    format!("{:.0} МБ", b as f64 / 1_048_576.0)
}

fn main() {
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleTitleW(windows::core::w!("Clipback"));
    }
    if let Err(e) = real_main() {
        eprintln!("Ошибка: {e:#}");
        // при запуске с ярлыка окно иначе сразу закроется и ошибку не прочитать
        eprintln!("\nНажмите Enter, чтобы закрыть.");
        let _ = std::io::stdin().read_line(&mut String::new());
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };
    let cfg = args.cfg.clone();

    let (rec, started) = Recorder::start(cfg.clone())?;
    if let Event::Started { encoder, gpu, monitor, width, height, fps, audio, audio_error } = started {
        println!("Clipback пишет последние {} мин", cfg.buffer_secs as f64 / 60.0);
        println!("  экран:      {monitor}, {width}×{height}, {fps} к/с");
        println!("  видеокарта: {gpu}");
        println!("  кодировщик: {encoder}, {} Мбит/с", cfg.bitrate_mbps);
        println!("  папка:      {}", cfg.out_dir.display());
        match audio_error {
            Some(e) => println!("  звук:       не работает — {e}"),
            None if audio.is_empty() => println!("  звук:       выключен"),
            None => println!("  звук:       {}", audio.join(", ")),
        }
    }

    let tx = rec.sender();
    // имя клипа — по окну, которое в фокусе в момент нажатия (обычно это игра)
    let save = move || {
        let label = clipback_core::util::foreground_window().and_then(|(t, _)| clipback_core::util::clip_label(&t));
        let _ = tx.send(clipback_core::Command::Save { seconds: None, label });
    };

    match hotkey::spawn(&args.hotkey, save.clone()) {
        Ok(()) => println!("Сохранить повтор: {}  (или Enter в этом окне)", args.hotkey),
        Err(e) => println!("Горячая клавиша не работает: {e:#}\nСохранить можно клавишей Enter в этом окне."),
    }

    // Enter в консоли — тоже сохранить; q — выйти
    let tx_quit = rec.sender();
    let save_console = save.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().eq_ignore_ascii_case("q") {
                let _ = tx_quit.send(clipback_core::Command::Stop);
                break;
            }
            save_console();
        }
    });

    if let Some(secs) = args.save_after {
        let tx = rec.sender();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            let _ = tx.send(clipback_core::Command::Save { seconds: None, label: None });
        });
    }

    for ev in rec.events.iter() {
        match ev {
            Event::Status { buffered_secs, buffer_bytes, fps, dropped } => {
                let m = buffered_secs as u64 / 60;
                let s = buffered_secs as u64 % 60;
                println!("  буфер {m}:{s:02} · {} в памяти · {fps:.0} к/с · пропущено {dropped}", mb(buffer_bytes as u64));
            }
            Event::Saved { path, seconds, bytes } => {
                println!("✔ Сохранено {:.1} с, {}: {}", seconds, mb(bytes), path.display());
                if args.save_after.is_some() {
                    break;
                }
            }
            Event::SaveFailed { message: e } => {
                println!("✘ Не удалось сохранить: {e}");
                if args.save_after.is_some() {
                    break;
                }
            }
            Event::Error { message } => bail!("{message}"),
            // полноценную запись CLI не запускает
            Event::Started { .. } | Event::RecordStarted { .. } | Event::Recorded { .. } | Event::RecordFailed { .. } => {}
        }
    }
    Ok(())
}
