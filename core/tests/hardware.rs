//! Проверка всего конвейера на настоящей видеокарте и мониторе.
//! По умолчанию не запускается (нужен рабочий стол и аппаратный кодировщик):
//!   cargo test --release -- --ignored

use clipback_core::{mux, Command, Config, Event, Recorder};
use std::time::{Duration, Instant};

/// Пишет файл, где щелчок звука стоит ровно на 1.000 с. Где он окажется после
/// декодирования (например, ffmpeg), — столько задержки добавляет AAC-кодировщик.
#[test]
#[ignore]
fn aac_alignment_probe() {
    use clipback_core::aac::{silent_frame, AacEncoder};
    use clipback_core::encoder::Packet;
    use std::sync::Arc;
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        windows::Win32::Media::MediaFoundation::MFStartup(windows::Win32::Media::MediaFoundation::MF_VERSION, 0).unwrap();
    }
    let mut enc = AacEncoder::new().unwrap();
    let mut pk = Vec::new();
    let rate = 48_000usize;
    let mut pcm = vec![0.0f32; rate * 3 * 2];
    for i in 0..480 {
        let v = (i as f32 * 0.13).sin() * 0.8;
        pcm[(rate + i) * 2] = v;
        pcm[(rate + i) * 2 + 1] = v;
    }
    for (n, chunk) in pcm.chunks(960 * 2).enumerate() {
        enc.encode((n * 960) as i64, chunk, &mut pk).unwrap();
    }
    let audio: Vec<_> = pk.into_iter().map(Arc::new).collect();
    // фиктивное видео 2.5 с (ffmpeg его не декодирует, звук — да)
    let sps = [0x67u8, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78];
    let pps = [0x68u8, 0xeb, 0xe3, 0xcb];
    let video: Vec<_> = (0..150)
        .map(|i| {
            let mut d = vec![0, 0, 0, 1];
            d.extend_from_slice(&sps);
            d.extend_from_slice(&[0, 0, 0, 1]);
            d.extend_from_slice(&pps);
            d.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0, 0]);
            Arc::new(Packet { data: d, pts: i * 10_000_000 / 60, key: true })
        })
        .collect();
    let silence = silent_frame().unwrap();
    let path = std::env::temp_dir().join("clipback-aac-probe.mp4");
    clipback_core::mux::write_av(&path, &video, &[clipback_core::mux::AudioInput { packets: &audio, silence: &silence }], 640, 360).unwrap();
    println!("PROBE {}", path.display());
}

/// Диагностика: что реально приходит от микрофона и системного звука.
#[test]
#[ignore]
fn audio_sources_deliver_sound() {
    use clipback_core::audio::{list_mics, qpc_now, spawn_source, SourceKind};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    for m in list_mics().unwrap() {
        println!("микрофон: {}{} — выключен: {}, громкость {:.0}%", m.name, if m.is_default { " (основной)" } else { "" }, m.muted, m.volume * 100.0);
    }
    for a in clipback_core::audio::list_audio_apps().unwrap() {
        println!("приложение со звуком: {} ({})", a.name, a.exe);
    }
    let first_id = list_mics().unwrap().into_iter().next().map(|m| m.id);
    let mut kinds = vec![SourceKind::Mic(None), SourceKind::Mic(first_id), SourceKind::System];
    // CLIPBACK_TEST_APP=powershell.exe — проверить захват отдельного приложения
    if let Ok(app) = std::env::var("CLIPBACK_TEST_APP") {
        kinds.push(SourceKind::App(app));
    }
    for kind in kinds {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let t0 = qpc_now();
        let h = spawn_source(0, kind.clone(), t0, tx, stop.clone());
        std::thread::sleep(std::time::Duration::from_secs(3));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        h.join().unwrap();
        let chunks: Vec<_> = rx.try_iter().collect();
        let frames: usize = chunks.iter().map(|c| c.samples.len() / 2).sum();
        let peak = chunks.iter().flat_map(|c| c.samples.iter()).fold(0f32, |a, &b| a.max(b.abs()));
        let first = chunks.first().map(|c| c.pos);
        let last = chunks.last().map(|c| c.pos + c.samples.len() as i64 / 2);
        println!("{kind:?}: кусков {}, кадров {frames} ({:.2} с), пик {peak:.5}, позиции {first:?}..{last:?}", chunks.len(), frames as f64 / 48000.0);
    }
}

#[test]
#[ignore]
fn records_saves_and_trims() {
    let out = std::env::temp_dir().join("clipback-hw-test");
    let _ = std::fs::create_dir_all(&out);
    let cfg = Config { buffer_secs: 3, out_dir: out.clone(), ..Config::default() };
    let (rec, started) = Recorder::start(cfg).expect("захват не запустился");
    assert!(matches!(started, Event::Started { .. }));

    // буфер длиной 3 с: ждём больше, чтобы проверить, что лишнее выбрасывается
    std::thread::sleep(Duration::from_millis(4500));
    rec.sender().send(Command::Save { seconds: None, label: Some("hw-test".into()) }).unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let path = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rec.events.recv_timeout(left).expect("нет события о сохранении") {
            Event::Saved { path, seconds, .. } => {
                assert!((2.0..=3.2).contains(&seconds), "длина {seconds}");
                break path;
            }
            Event::SaveFailed { message } | Event::Error { message } => panic!("{message}"),
            _ => {}
        }
    };
    drop(rec);

    let info = mux::clip_info(&path).unwrap();
    assert!((2.0..=3.2).contains(&info.duration), "длина {}", info.duration);
    assert!(info.fps > 50.0, "fps {}", info.fps);
    assert!(info.width >= 640 && info.height >= 360);
    assert!(path.file_name().unwrap().to_string_lossy().starts_with("hw-test "));

    // по умолчанию пишется звук системы — одна дорожка
    assert_eq!(info.audio_tracks, 1, "звуковых дорожек");

    let cut = out.join("hw-test-cut.mp4");
    let r = mux::trim(&path, &cut, 1.2, 2.0).unwrap();
    assert!(r.start <= 1.2 && r.start >= 0.0);
    let cinfo = mux::clip_info(&cut).unwrap();
    assert!((cinfo.duration - (r.end - r.start)).abs() < 0.05);
    assert_eq!(cinfo.audio_tracks, 1);

    // отрезок без звука
    let mute = out.join("hw-test-mute.mp4");
    mux::trim_tracks(&path, &mute, 0.0, 2.0, Some(&[])).unwrap();
    assert_eq!(mux::clip_info(&mute).unwrap().audio_tracks, 0);

    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(cut);
    let _ = std::fs::remove_file(mute);
}

/// Перекодирование и форма волны на настоящем клипе:
///   CLIPBACK_TEST_CLIP=путь.mp4 cargo test --release --test hardware transcode -- --ignored --nocapture
#[test]
#[ignore]
fn transcode_and_peaks_on_real_clip() {
    use clipback_core::media::{audio_peaks, transcode, Quality};
    let src = std::path::PathBuf::from(std::env::var("CLIPBACK_TEST_CLIP").expect("укажите CLIPBACK_TEST_CLIP"));
    let info = mux::clip_info(&src).unwrap();
    println!("исходник: {:.1} с, {}×{}, {:.0} к/с, дорожки {:?}", info.duration, info.width, info.height, info.fps, info.audio_names);

    let t = Instant::now();
    let peaks = audio_peaks(&src, 400, info.duration).unwrap();
    println!("форма волны: {} дорожек за {:?}, макс {:?}", peaks.len(), t.elapsed(), peaks.iter().map(|p| p.iter().cloned().fold(0f32, f32::max)).collect::<Vec<_>>());
    assert_eq!(peaks.len(), info.audio_tracks);

    for q in ["high", "medium", "low"] {
        let dst = std::env::temp_dir().join(format!("clipback-transcode-{q}.mp4"));
        let t = Instant::now();
        let r = transcode(&src, &dst, 3.0, 9.0, None, &Quality::preset(q).unwrap(), &mut |_| {}).unwrap();
        let o = mux::clip_info(&dst).unwrap();
        let size = std::fs::metadata(&dst).unwrap().len() as f64 / 1048576.0;
        println!("{q}: {:.2}–{:.2} с за {:?} → {:.2} с, {}×{}, {:.0} к/с, {size:.1} МБ, дорожки {:?}", r.start, r.end, t.elapsed(), o.duration, o.width, o.height, o.fps, o.audio_names);
        assert!((r.start - 3.0).abs() < 0.05, "перекодирование режет точно по кадру");
        assert!((o.duration - 6.0).abs() < 0.1);
        assert_eq!(o.audio_names, info.audio_names);
        assert!(o.height <= Quality::preset(q).unwrap().max_height);
    }
    // без микрофона и общей дорожки
    let dst = std::env::temp_dir().join("clipback-transcode-nomic.mp4");
    transcode(&src, &dst, 0.0, 2.0, Some(&[1]), &Quality::preset("low").unwrap(), &mut |_| {}).unwrap();
    assert_eq!(mux::clip_info(&dst).unwrap().audio_tracks, 1);
}

