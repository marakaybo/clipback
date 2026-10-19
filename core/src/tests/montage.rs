use super::*;

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

/// Синтетическое видео: `secs` секунд, 60 к/с, ключевой кадр каждые 60 кадров.
fn packets(secs: u32) -> Vec<Arc<Packet>> {
    (0..secs * 60)
        .map(|i| Arc::new(Packet { data: frame(i % 60 == 0, i as u8), pts: i as i64 * 10_000_000 / 60, key: i % 60 == 0 }))
        .collect()
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("clipback-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Кадры 60 к/с: начала и конец последнего.
fn times(secs: i64) -> (Vec<i64>, i64) {
    let n = secs * 60;
    ((0..n).map(|k| k * 10_000_000 / 60).collect(), n * 10_000_000 / 60)
}

#[test]
fn plan_takes_frames_by_their_middle() {
    let (t, end) = times(10);
    let p = plan(&t, end, &[(0.5, 1.0), (2.0, 2.5)]);
    assert_eq!(p.len(), 2);
    assert_eq!((p[0].frames, p[1].frames), (30, 30));
    assert_eq!(p[0].src_start, t[30]);
    assert_eq!(p[0].src_end, t[60]);
    assert_eq!(p[1].out_start, p[0].src_end - p[0].src_start);
    assert_eq!(total(&p), t[60] - t[0]);
}

#[test]
fn cut_inside_a_frame_gives_it_to_one_side_only() {
    let (t, end) = times(10);
    // 0,508 с — внутри кадра 30 (0,5–0,5167), до его середины
    let a = plan(&t, end, &[(0.0, 0.508)]);
    let b = plan(&t, end, &[(0.508, 1.0)]);
    assert_eq!(a[0].frames + b[0].frames, 60);
    assert_eq!(a[0].src_end, b[0].src_start, "без пропуска и без повтора");
    // соседние куски после разреза — снова один кусок
    let ab = plan(&t, end, &[(0.508, 1.0), (0.0, 0.508)]);
    assert_eq!(ab.len(), 1);
    assert_eq!(ab[0].frames, 60);
}

#[test]
fn plan_merges_overlaps_and_drops_empty() {
    let (t, end) = times(10);
    let p = plan(&t, end, &[(3.0, 5.0), (1.0, 2.0), (4.0, 6.0), (7.0, 7.0), (8.0, 7.5), (f64::NAN, 1.0), (9.5, 99.0)]);
    assert_eq!(p.iter().map(|x| x.frames).collect::<Vec<_>>(), vec![60, 180, 30]);
    assert_eq!(p[2].src_end, end, "за концом клипа — до последнего кадра");
    assert_eq!(total(&p), (t[120] - t[60]) + (t[360] - t[180]) + (end - t[570]));
    assert!(plan(&t, end, &[(0.001, 0.002)]).is_empty(), "меньше кадра — пусто");
    assert!(plan(&t, end, &[]).is_empty());
}

#[test]
fn audio_boundaries_do_not_drift() {
    // 1000 кусков по кадру: сумма длин в отсчётах — ровно длина всего
    let (t, end) = times(40);
    let ranges: Vec<(f64, f64)> = (0..1000).map(|k| (k as f64 * 2.0 / 60.0, (k as f64 * 2.0 + 1.0) / 60.0)).collect();
    let p = plan(&t, end, &ranges);
    assert_eq!(p.len(), 1000);
    let sum: i64 = p.iter().map(|x| samples_at(x.out_start + x.src_end - x.src_start) - samples_at(x.out_start)).sum();
    assert_eq!(sum, samples_at(total(&p)));
    assert_eq!(samples_at(total(&p)), 1000 * 800, "1000 кадров 60 к/с = 16,67 с = 800 000 отсчётов");
}

#[test]
fn take_pads_with_silence() {
    let pcm: Vec<f32> = (0..10).flat_map(|k| [k as f32, -(k as f32)]).collect();
    // звук начинается с позиции 100; берём 4 отсчёта с 98
    let v = take(100, &pcm, 98, 4);
    assert_eq!(v, vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, -1.0]);
    assert_eq!(take(100, &pcm, 108, 4), vec![8.0, -8.0, 9.0, -9.0, 0.0, 0.0, 0.0, 0.0]);
}

fn audio(secs: i64) -> Vec<Arc<AudioPacket>> {
    (0..secs * 48_000 / 1024 + 1).map(|k| Arc::new(AudioPacket { data: vec![0x21, k as u8, 7], pos: k * 1024 })).collect()
}

#[test]
fn stream_writer_and_faststart_keep_every_sample() {
    let part = tmp("montage-stream.part");
    let out = tmp("montage-stream.mp4");
    let total = 3 * 10_000_000;
    let a = [audio(3), audio(3)];
    let names = vec!["Звук системы".to_string(), "Микрофон".to_string()];
    let mut w = StreamWriter::new(&part, 64, 32, &a, &[0x21, 0, 0], total);
    // кодировщик может отдать пакеты чуть не по порядку
    let mut ps: Vec<Packet> = packets(3).iter().map(|p| Packet { data: p.data.clone(), pts: p.pts, key: p.key }).collect();
    ps.swap(5, 6);
    for p in ps {
        w.push(p).unwrap();
    }
    w.finish(&names).unwrap();
    faststart(&part, &out).unwrap();

    let mut f = std::fs::File::open(&out).unwrap();
    let order: Vec<[u8; 4]> = top_boxes(&mut f).unwrap().into_iter().map(|b| b.0).collect();
    let pos = |k: &[u8; 4]| order.iter().position(|x| x == k).unwrap();
    assert!(pos(b"moov") < pos(b"mdat"), "{order:?}");
    assert_eq!(mux::read_track_names(&out), Some(names));

    let mut r = mux::open(&out).unwrap();
    let vid = mux::video_track_id(&r).unwrap();
    assert_eq!(r.tracks()[&vid].sample_count(), 180);
    for k in [1u32, 6, 7, 180] {
        let s = r.read_sample(vid, k).unwrap().unwrap();
        let want = mux::annexb_to_avcc(&frame((k - 1) % 60 == 0, (k - 1) as u8));
        assert_eq!(s.bytes.as_ref(), want.as_slice(), "кадр {k}");
        assert_eq!(s.start_time, (k as u64 - 1) * 1500);
    }
    // звук — до конца видео (3 с = 140,6 кадра AAC), как у обычного сохранения
    for id in [2u32, 3] {
        assert_eq!(r.tracks()[&id].sample_count(), 141);
        let s = r.read_sample(id, 10).unwrap().unwrap();
        assert_eq!(s.bytes.as_ref(), &[0x21, 9, 7]);
    }
    let info = mux::clip_info(&out).unwrap();
    assert!((info.duration - 3.0).abs() < 0.02, "{}", info.duration);
    let _ = std::fs::remove_file(&part);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn faststart_keeps_file_already_in_order() {
    let a = tmp("montage-fs-a.mp4");
    let b = tmp("montage-fs-b.mp4");
    let c = tmp("montage-fs-c.mp4");
    mux::write_mp4(&a, &packets(1), 64, 32).unwrap();
    faststart(&a, &b).unwrap();
    faststart(&b, &c).unwrap();
    assert_eq!(std::fs::read(&b).unwrap(), std::fs::read(&c).unwrap());
    assert_eq!(std::fs::metadata(&a).unwrap().len(), std::fs::metadata(&b).unwrap().len());
    for p in [a, b, c] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn frame_times_match_the_file() {
    let p = tmp("montage-times.mp4");
    mux::write_mp4(&p, &packets(2), 64, 32).unwrap();
    let (t, end) = frame_times(&p).unwrap();
    assert_eq!(t.len(), 120);
    assert_eq!(t[60], 10_000_000);
    assert_eq!(end, 20_000_000);
    let _ = std::fs::remove_file(&p);
}

/// Настоящий клип через видеокарту: CLIPBACK_TEST_CLIP=путь cargo test -- --ignored real_clip
#[test]
#[ignore]
fn real_clip_montage() {
    let Ok(src) = std::env::var("CLIPBACK_TEST_CLIP") else { return };
    let src = PathBuf::from(src);
    let dst = tmp("montage-real.mp4");
    let ranges = [(1.0, 3.0), (5.0, 6.5), (8.0, 10.25)];
    let q = source_quality(&src).unwrap();
    let r = montage(&src, &dst, &ranges, None, Some(&[1.0, 0.5, 2.0]), &q, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    let info = mux::clip_info(&src).unwrap();
    let out = mux::clip_info(&dst).unwrap();
    let frame = 1.0 / info.fps;
    assert_eq!(r.pieces, 3);
    assert!((out.duration - 5.75).abs() <= frame + 1e-3, "{}", out.duration);
    assert!((r.duration - out.duration).abs() < 1e-3);
    assert_eq!(out.audio_names, info.audio_names);
    assert_eq!((out.width, out.height), (info.width, info.height));
    // оглавление впереди
    let mut f = std::fs::File::open(&dst).unwrap();
    let b = top_boxes(&mut f).unwrap();
    assert!(b.iter().position(|x| &x.0 == b"moov") < b.iter().position(|x| &x.0 == b"mdat"));
    drop(f);
    // отмена: файла нет, временных тоже
    let cancelled = tmp("montage-cancel.mp4");
    let e = montage(&src, &cancelled, &ranges, None, None, &q, &mut |_| {}, &AtomicBool::new(true)).unwrap_err();
    assert_eq!(e.to_string(), "отменено");
    assert!(!cancelled.exists() && !cancelled.with_extension("part").exists() && !cancelled.with_extension("fs.part").exists());
    let _ = std::fs::remove_file(&dst);
}
