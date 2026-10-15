use super::*;

#[test]
fn resampler_passthrough_at_48k() {
    let mut r = Resampler::new(48_000);
    let mut out = Vec::new();
    r.process(&[(0.1, 0.2), (0.3, 0.4)], &mut out);
    assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
}

#[test]
fn resampler_44k_to_48k_keeps_duration() {
    let mut r = Resampler::new(44_100);
    let mut out = Vec::new();
    // 1 секунда кусками по 441 кадру, как приходит от устройства
    for _ in 0..100 {
        let chunk: Vec<(f32, f32)> = (0..441).map(|_| (0.5, -0.5)).collect();
        r.process(&chunk, &mut out);
    }
    let frames = out.len() / 2;
    assert!((47_990..=48_010).contains(&frames), "кадров {frames}");
    // постоянный сигнал остаётся постоянным (кроме самого первого отсчёта)
    assert!(out[200..].chunks(2).all(|f| (f[0] - 0.5).abs() < 1e-5 && (f[1] + 0.5).abs() < 1e-5));
}

#[test]
fn resampler_96k_halves() {
    let mut r = Resampler::new(96_000);
    let mut out = Vec::new();
    let chunk: Vec<(f32, f32)> = (0..9600).map(|i| (i as f32, 0.0)).collect();
    r.process(&chunk, &mut out);
    assert_eq!(out.len() / 2, 4800);
}

#[test]
fn align_fills_gaps_and_drops_overlaps() {
    assert_eq!(align(1000, 1005, 480), (0, 0), "мелкое дрожание игнорируем");
    assert_eq!(align(1000, 2000, 480), (1000, 0), "пропуск — тишина");
    assert_eq!(align(2000, 1000, 480), (0, 1000), "наложение — отбрасываем");
}

#[test]
fn stereo_from_formats() {
    let f = |v: &[f32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    assert_eq!(frame_to_stereo(Fmt::F32, &f(&[0.25, -0.25]), 2), (0.25, -0.25));
    assert_eq!(frame_to_stereo(Fmt::F32, &f(&[0.5]), 1), (0.5, 0.5));
    let i16s: Vec<u8> = [16384i16, -16384].iter().flat_map(|x| x.to_le_bytes()).collect();
    assert_eq!(frame_to_stereo(Fmt::I16, &i16s, 2), (0.5, -0.5));
    // 24 бит: 0x400000 = 0.5
    assert_eq!(frame_to_stereo(Fmt::I24, &[0, 0, 0x40, 0, 0, 0xc0], 2), (0.5, -0.5));
    // 5.1: центр подмешивается в обе стороны
    let (l, r) = frame_to_stereo(Fmt::F32, &f(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0]), 6);
    assert!((l - 0.707).abs() < 1e-3 && (r - 0.707).abs() < 1e-3);
}
