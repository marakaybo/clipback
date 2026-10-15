use super::*;

const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78];
const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];

fn frame(key: bool, n: u8) -> Vec<u8> {
    let mut d = vec![0, 0, 0, 1, 0x09, 0xf0]; // AUD
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

/// Синтетический клип: `secs` секунд, 60 к/с, ключевой кадр каждые 60 кадров.
fn packets(secs: u32) -> Vec<Arc<Packet>> {
    (0..secs * 60)
        .map(|i| Arc::new(Packet { data: frame(i % 60 == 0, i as u8), pts: i as i64 * 10_000_000 / 60, key: i % 60 == 0 }))
        .collect()
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("clipback-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn splits_nals_with_both_start_codes() {
    let data = frame(true, 7);
    let nals = split_nals(&data);
    assert_eq!(nals.len(), 4);
    assert_eq!(nals[0][0] & 0x1f, 9);
    assert_eq!(nals[1], SPS);
    assert_eq!(nals[2], PPS);
    assert_eq!(nals[3][0] & 0x1f, 5);
}

#[test]
fn split_ignores_garbage_without_start_code() {
    assert!(split_nals(&[1, 2, 3, 4]).is_empty());
    assert!(split_nals(&[]).is_empty());
}

#[test]
fn avcc_keeps_only_picture_nals() {
    let avcc = annexb_to_avcc(&frame(true, 1));
    // длина 4 + сам IDR-срез (4 байта)
    assert_eq!(avcc, vec![0, 0, 0, 4, 0x65, 0x88, 1, 1]);
}

#[test]
fn finds_sps_pps() {
    let (s, p) = find_sps_pps(&frame(true, 0)).unwrap();
    assert_eq!(s, SPS);
    assert_eq!(p, PPS);
    assert!(find_sps_pps(&frame(false, 0)).is_none());
}

#[test]
fn writes_and_reads_back_clip() {
    let path = tmp("write.mp4");
    write_mp4(&path, &packets(5), 1920, 1080).unwrap();
    assert!(!path.with_extension("part").exists(), "временный файл должен исчезнуть");
    let info = clip_info(&path).unwrap();
    assert!((info.duration - 5.0).abs() < 0.05, "длина {}", info.duration);
    assert_eq!((info.width, info.height), (1920, 1080));
    assert!((info.fps - 60.0).abs() < 0.5, "fps {}", info.fps);
}

#[test]
fn refuses_empty_buffer() {
    assert!(write_mp4(&tmp("empty.mp4"), &[], 1920, 1080).is_err());
}

#[test]
fn trims_from_previous_keyframe() {
    let src = tmp("trim-src.mp4");
    let dst = tmp("trim-dst.mp4");
    write_mp4(&src, &packets(5), 1280, 720).unwrap();
    let r = trim(&src, &dst, 1.5, 3.25).unwrap();
    assert!((r.start - 1.0).abs() < 1e-6, "начало должно уйти к ключевому кадру 1.0, а не {}", r.start);
    assert!((r.end - 3.25).abs() < 0.02, "конец {}", r.end);
    let info = clip_info(&dst).unwrap();
    assert!((info.duration - (r.end - r.start)).abs() < 0.02, "длина {}", info.duration);
    // первый кадр отрезка — ключевой и начинается с нуля
    let mut rd = open(&dst).unwrap();
    let id = video_track_id(&rd).unwrap();
    let s = rd.read_sample(id, 1).unwrap().unwrap();
    assert!(s.is_sync);
    assert_eq!(s.start_time, 0);
}

#[test]
fn trim_at_exact_keyframe() {
    let src = tmp("trim2-src.mp4");
    let dst = tmp("trim2-dst.mp4");
    write_mp4(&src, &packets(4), 640, 360).unwrap();
    let r = trim(&src, &dst, 2.0, 4.0).unwrap();
    assert!((r.start - 2.0).abs() < 1e-6);
    assert!((r.end - 4.0).abs() < 0.02);
}

#[test]
fn trim_can_replace_source() {
    let p = tmp("trim-replace.mp4");
    write_mp4(&p, &packets(3), 640, 360).unwrap();
    trim(&p, &p, 0.0, 1.0).unwrap();
    let info = clip_info(&p).unwrap();
    assert!((info.duration - 1.0).abs() < 0.03, "длина {}", info.duration);
}

#[test]
fn trim_rejects_bad_range() {
    let src = tmp("trim3-src.mp4");
    write_mp4(&src, &packets(2), 640, 360).unwrap();
    assert!(trim(&src, &tmp("trim3-dst.mp4"), 1.5, 1.0).is_err());
    assert!(trim(&src, &tmp("trim3-dst.mp4"), 10.0, 12.0).is_err());
}

#[test]
fn clip_info_rejects_non_mp4() {
    let p = tmp("not-video.mp4");
    std::fs::write(&p, b"hello").unwrap();
    assert!(clip_info(&p).is_err());
}

#[test]
fn track_names_roundtrip_and_file_stays_valid() {
    let p = tmp("names.mp4");
    write_mp4(&p, &packets(2), 640, 360).unwrap();
    assert_eq!(read_track_names(&p), None);
    let names = vec!["Всё вместе".to_string(), "Discord".to_string(), "Микрофон".to_string()];
    write_track_names(&p, &names).unwrap();
    assert_eq!(read_track_names(&p), Some(names));
    // служебный блок не мешает читать файл
    let info = clip_info(&p).unwrap();
    assert!((info.duration - 2.0).abs() < 0.05);
    assert!(info.audio_names.is_empty(), "звуковых дорожек нет — названия не применяются");
}

#[test]
fn default_names_by_count() {
    assert_eq!(default_track_names(3)[2], "Микрофон");
    assert_eq!(default_track_names(1), vec!["Звук"]);
    assert_eq!(default_track_names(2), vec!["Дорожка 1", "Дорожка 2"]);
}
