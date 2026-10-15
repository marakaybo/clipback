use super::*;
use crate::mux::clip_info;

const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78];
const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];

fn frame(key: bool, n: u8) -> Vec<u8> {
    let mut d = vec![0, 0, 0, 1, 0x09, 0xf0];
    if key {
        d.extend_from_slice(&[0, 0, 0, 1]);
        d.extend_from_slice(SPS);
        d.extend_from_slice(&[0, 0, 1]);
        d.extend_from_slice(PPS);
        d.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, n, n]);
    } else {
        d.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9a, n]);
    }
    d
}

/// Кадр `i` при 60 к/с, ключевой каждые 60, время сдвинуто на `base` (как у настоящего захвата).
fn pkt(i: u32, base: i64) -> Arc<Packet> {
    Arc::new(Packet { data: frame(i % 60 == 0, i as u8), pts: base + i as i64 * 10_000_000 / 60, key: i % 60 == 0 })
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("clipback-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    p
}

fn silence() -> Vec<u8> {
    vec![0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c]
}

#[test]
fn writes_video_and_audio_as_it_goes() {
    let path = tmp("live.mp4");
    let base = 50_000_000; // запись началась через 5 с после старта захвата
    let names = vec!["Звук системы".to_string(), "Микрофон".to_string()];
    let mut w = LiveWriter::create(&path, &pkt(0, base), &names, &silence(), 1280, 720).unwrap();
    let t0 = base * crate::audio::RATE as i64 / 10_000_000;
    let mut apos = t0 - 2048; // немного звука до первого кадра — отбрасывается
    for i in 0..180 {
        w.video(pkt(i, base)).unwrap();
        // звук приходит пачками вперемешку с видео
        while ring_time(apos) <= base + i as i64 * 10_000_000 / 60 {
            w.audio(0, &AudioPacket { data: vec![1, 2, 3], pos: apos }).unwrap();
            // у микрофона пропуск во второй секунде
            if !(t0 + 48_000..t0 + 96_000).contains(&apos) {
                w.audio(1, &AudioPacket { data: vec![4, 5, 6], pos: apos }).unwrap();
            }
            apos += crate::aac::FRAME;
        }
    }
    let (out, secs) = w.finish().unwrap();
    assert_eq!(out, path);
    assert!((secs - 3.0).abs() < 0.02, "{secs}");
    assert!(!path.with_extension("part").exists());

    let info = clip_info(&path).unwrap();
    assert!((info.duration - 3.0).abs() < 0.05, "{}", info.duration);
    assert_eq!(info.audio_tracks, 2);
    assert_eq!(info.audio_names, names);

    // в дорожке микрофона пропуск заполнен тишиной: кадров столько же, сколько у системы
    let r = mp4::Mp4Reader::read_header(std::io::BufReader::new(std::fs::File::open(&path).unwrap()), std::fs::metadata(&path).unwrap().len()).unwrap();
    let counts: Vec<u32> = (2..=3).map(|id| r.sample_count(id).unwrap()).collect();
    assert_eq!(counts[0], counts[1]);
    assert!(counts[0] as i64 * crate::aac::FRAME >= 3 * 48_000 - crate::aac::FRAME);
}

fn ring_time(pos: i64) -> i64 {
    crate::ring::audio_time(pos)
}

#[test]
fn background_recording_starts_from_key_frame_and_finishes_on_stop() {
    let path = tmp("live-bg.mp4");
    // в буфере начало группы без ключевого кадра — его нужно пропустить
    let pre: Vec<Arc<Packet>> = (30..70).map(|i| pkt(i, 0)).collect();
    let (tx, rx) = mpsc::channel();
    let rec = LiveRec::start(
        path.clone(),
        vec![],
        Arc::new(silence()),
        640,
        360,
        (pre, vec![]),
        Box::new(move |r| {
            let _ = tx.send(r.map_err(|e| e.to_string()));
        }),
    );
    for i in 70..240 {
        rec.send(Msg::Video(pkt(i, 0)));
    }
    let t = rec.stop().unwrap();
    t.join().unwrap();
    let (p, secs, bytes) = rx.recv().unwrap().unwrap();
    assert_eq!(p, path);
    // с кадра 60 по 239 — 3 секунды
    assert!((secs - 3.0).abs() < 0.02, "{secs}");
    assert!(bytes > 0);
    assert_eq!(clip_info(&path).unwrap().audio_tracks, 0);
}

#[test]
fn recording_without_frames_reports_error() {
    let path = tmp("live-empty.mp4");
    let (tx, rx) = mpsc::channel();
    let rec = LiveRec::start(path.clone(), vec![], Arc::new(silence()), 640, 360, (vec![], vec![]), Box::new(move |r| {
        let _ = tx.send(r.is_err());
    }));
    drop(rec);
    assert!(rx.recv().unwrap());
    assert!(!path.exists());
}
