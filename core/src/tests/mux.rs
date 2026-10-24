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

/// Звуковая дорожка из PCM (стерео подряд): кадры AAC с позициями от нуля.
fn aac(pcm: &[f32]) -> Vec<Arc<AudioPacket>> {
    let mut enc = crate::aac::AacEncoder::new().unwrap();
    let mut out = Vec::new();
    for (k, c) in pcm.chunks(48_000 * 2).enumerate() {
        enc.encode(k as i64 * 48_000, c, &mut out).unwrap();
    }
    enc.encode((pcm.len() / 2) as i64, &vec![0.0; 1024 * 8], &mut out).unwrap();
    out.into_iter().map(Arc::new).collect()
}

fn noise(a: f32, secs: usize) -> Vec<f32> {
    let mut s = 7u32;
    (0..secs * 48_000).flat_map(|_| {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        let x = a * ((s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0);
        [x, x]
    }).collect()
}

fn tone(a: f32, secs: usize) -> Vec<f32> {
    (0..secs * 48_000).flat_map(|i| {
        let x = a * (i as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin();
        [x, x]
    }).collect()
}

fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
}

/// Клип «как из старой версии»: «Всё вместе» = система + микрофон без выравнивания, голос на 12 дБ тише.
fn old_style_clip(name: &str, secs: usize, named: bool) -> std::path::PathBuf {
    let p = tmp(name);
    let (sys, mic) = (noise(0.4, secs), tone(0.1, secs));
    let mix: Vec<f32> = sys.iter().zip(&mic).map(|(a, b)| a + b).collect();
    let (a, b, c) = (aac(&mix), aac(&sys), aac(&mic));
    let silence = crate::aac::silent_frame().unwrap();
    let inputs = [&a, &b, &c].map(|t| AudioInput { packets: t, silence: &silence });
    let names: Vec<String> = if named { default_track_names(3) } else { vec![] };
    write_av_named(&p, &packets(secs as u32), &inputs, &names, 640, 360).unwrap();
    p
}

/// Звуковые дорожки: (флаги tkhd, группа «одна из», название в hdlr).
fn audio_traks(p: &std::path::Path) -> Vec<(u32, u16, String)> {
    let r = open(p).unwrap();
    audio_track_ids(&r)
        .iter()
        .map(|id| {
            let t = &r.tracks()[id].trak;
            (t.tkhd.flags, t.tkhd.alternate_group, t.mdia.hdlr.name.trim_end_matches('\0').to_string())
        })
        .collect()
}

#[test]
fn only_first_audio_track_is_default_and_all_are_named() {
    let _mf = crate::media::MfScope::new().unwrap();
    let p = old_style_clip("labels.mp4", 2, true);
    let t = audio_traks(&p);
    assert_eq!(t.len(), 3);
    assert_eq!(t.iter().map(|t| t.0).collect::<Vec<_>>(), vec![3, 2, 2], "включена только «Всё вместе»");
    assert!(t.iter().all(|t| t.1 == 1), "звуковые дорожки — «одна из»");
    let hdlr: Vec<String> = t.iter().map(|t| t.2.clone()).collect();
    assert_eq!(hdlr, default_track_names(3));
    assert_eq!(read_track_names(&p), Some(default_track_names(3)));
    assert!(mix_leveled(&p), "новый файл помечен: «Всё вместе» уже выровнена");
    // файл по-прежнему читается: кадры на месте
    let mut r = open(&p).unwrap();
    let ids = audio_track_ids(&r);
    assert!(r.read_sample(ids[2], 1).unwrap().is_some());
    assert!((clip_info(&p).unwrap().duration - 2.0).abs() < 0.05);
}

#[test]
fn old_clip_mix_is_rebuilt_only_when_needed() {
    let _mf = crate::media::MfScope::new().unwrap();
    let p = old_style_clip("old-mix.mp4", 2, true);
    assert!(!crate::media::audio_rework(&p, None, None), "новый клип без изменений — копия кадров");
    // убрать метку — как у клипа из 0.5.5
    let d = std::fs::read(&p).unwrap();
    let at = d.windows(12).rposition(|w| w == b"clipback-mix").unwrap() - 8;
    std::fs::write(&p, &d[..at]).unwrap();
    assert!(!mix_leveled(&p));
    assert!(crate::media::audio_rework(&p, None, None), "старый «Всё вместе» сводится заново");
    assert!(!crate::media::audio_rework(&p, Some(&[1, 2]), None), "без «Всё вместе» — копия");
}

#[test]
fn one_track_copy_has_leveled_voice_and_faststart() {
    let _mf = crate::media::MfScope::new().unwrap();
    let src = old_style_clip("one-src.mp4", 6, false);
    assert!(!mix_leveled(&src));
    let dst = tmp("one-dst.mp4");
    let mut seen = Vec::new();
    one_track(&src, &dst, None, &mut |f| seen.push(f), &crate::media::NEVER).unwrap();
    assert!(seen.windows(2).all(|w| w[1] >= w[0] - 1e-9), "ход работы не идёт назад: {seen:?}");
    assert_eq!(seen.last(), Some(&1.0));
    assert!(seen.len() >= 4, "{seen:?}");
    assert!(!dst.with_extension("1t.mp4").exists() && !dst.with_extension("fs.part").exists());
    let info = clip_info(&dst).unwrap();
    assert_eq!(info.audio_tracks, 1);
    assert_eq!(info.audio_names, vec!["Всё вместе".to_string()]);
    assert!(mix_leveled(&dst));
    let d = std::fs::read(&dst).unwrap();
    let pos = |t: &[u8]| d.windows(4).position(|w| w == t).unwrap();
    assert!(pos(b"moov") < pos(b"mdat"), "оглавление в начале файла");
    // голос подтянут: в конце (выравнивание уже сошлось) звук заметно громче старой суммы
    let (old, _) = read_audio_segment(&src, 4.0, 6.0, Some(&[0])).unwrap();
    let (new, _) = read_audio_segment(&dst, 4.0, 6.0, None).unwrap();
    let (_, old) = crate::media::decode_track(&old[0]).unwrap();
    let (_, new) = crate::media::decode_track(&new[0]).unwrap();
    assert!(rms(&new) > rms(&old) * 1.25, "rms {} → {}", rms(&old), rms(&new));
    assert!(new.iter().all(|x| x.abs() <= 1.0));
}

#[test]
fn trim_reports_progress_and_can_be_cancelled() {
    let src = tmp("prog-src.mp4");
    write_mp4(&src, &packets(5), 640, 360).unwrap();
    let mut seen = Vec::new();
    trim_tracks_with(&src, &tmp("prog-dst.mp4"), 0.0, 5.0, None, None, &mut |f| seen.push(f), &crate::media::NEVER).unwrap();
    assert_eq!(seen.first(), Some(&0.0));
    assert_eq!(seen.last(), Some(&1.0));
    assert!(seen.len() >= 5);
    let stop = AtomicBool::new(true);
    let dst = tmp("prog-cancel.mp4");
    let _ = std::fs::remove_file(&dst);
    let e = trim_tracks_with(&src, &dst, 0.0, 5.0, None, None, &mut |_| {}, &stop).unwrap_err();
    assert_eq!(e.to_string(), "отменено");
    assert!(!dst.exists() && !dst.with_extension("part").exists(), "после отмены файлов не остаётся");
}
