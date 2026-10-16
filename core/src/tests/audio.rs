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

fn packet(n: usize, v: f32) -> Vec<f32> {
    vec![v; n * 2]
}

/// (позиция, кадров, тишина ли) — для проверок.
fn shape(out: &[(i64, Vec<f32>)]) -> Vec<(i64, i64, bool)> {
    out.iter().map(|(p, s)| (*p, s.len() as i64 / 2, s.iter().all(|&x| x == 0.0))).collect()
}

#[test]
fn placer_ignores_jitter() {
    let mut p = Placer::new();
    let mut out = Vec::new();
    p.place(1000, packet(480, 0.5), &mut out);
    p.place(1480 + 96, packet(480, 0.5), &mut out); // метка на 2 мс позже
    p.place(1960 - 960, packet(480, 0.5), &mut out); // и на 20 мс раньше
    assert_eq!(shape(&out), vec![(1000, 480, false), (1480, 480, false), (1960, 480, false)]);
}

#[test]
fn placer_fills_real_gaps() {
    let mut p = Placer::new();
    let mut out = Vec::new();
    p.place(0, packet(480, 0.5), &mut out);
    p.place(480 + 9600, packet(480, 0.5), &mut out); // устройство молчало 200 мс
    assert_eq!(shape(&out), vec![(0, 480, false), (480, 9600, true), (10080, 480, false)]);
}

#[test]
fn placer_drops_only_the_overlap() {
    let mut p = Placer::new();
    let mut out = Vec::new();
    p.place(0, packet(4800, 0.5), &mut out);
    // кусок пришёл с меткой на 100 мс раньше: первые 100 мс уже записаны
    p.place(0, packet(9600, 0.7), &mut out);
    assert_eq!(shape(&out), vec![(0, 4800, false), (4800, 4800, false)]);
    // и дальше звук идёт как обычно — раньше здесь микрофон пропадал навсегда
    out.clear();
    p.place(9600, packet(480, 0.5), &mut out);
    assert_eq!(shape(&out), vec![(9600, 480, false)]);
}

#[test]
fn placer_skips_before_start() {
    let mut p = Placer::new();
    let mut out = Vec::new();
    p.place(-300, packet(480, 0.5), &mut out);
    p.place(180, packet(480, 0.5), &mut out);
    assert_eq!(shape(&out), vec![(0, 180, false), (180, 480, false)]);
}

#[test]
fn fill_does_not_overrun_data() {
    let mut p = Placer::new();
    assert!(p.fill_to(5000).is_none(), "первый вызов только запоминает позицию");
    assert!(p.fill_to(5100).is_none(), "мелочь не дописываем");
    let (pos, s) = p.fill_to(20000).unwrap();
    assert_eq!((pos, s.len() / 2), (5000, 15000));
}

/// Часы микрофона спешат на 146 ppm (замерено на fifine): 10 минут звука — без единой
/// дыры и без выброшенных кусков, а расхождение со временем не растёт.
#[test]
fn placer_follows_fast_mic_clock() {
    follow_clock(146e-6);
}

#[test]
fn placer_follows_slow_mic_clock() {
    follow_clock(-400e-6);
}

fn follow_clock(ppm: f64) {
    let mut p = Placer::new();
    let mut rs = Resampler::new(48_000);
    let mut out = Vec::new();
    let mut expect = 0i64;
    let mut device_frames = 0f64;
    let mut worst = 0f64;
    for k in 0..60_000 {
        let input: Vec<(f32, f32)> = (0..480).map(|_| (0.25, 0.25)).collect();
        // метка — по системным часам: устройство отдаёт 480 кадров чуть быстрее, чем за 10 мс
        let start = (device_frames / (1.0 + ppm)) as i64 + if k % 7 == 3 { 40 } else { 0 };
        device_frames += 480.0;
        let mut s = Vec::new();
        rs.set_correction(p.correction());
        rs.process(&input, &mut s);
        out.clear();
        p.place(start, s, &mut out);
        for (pos, s) in &out {
            assert_eq!(*pos, expect, "дыра или наложение на пакете {k}");
            assert!(s.iter().all(|&x| x != 0.0), "вставлена тишина на пакете {k}");
            expect += s.len() as i64 / 2;
        }
        if k > 3000 {
            worst = worst.max((expect - (device_frames / (1.0 + ppm)) as i64).abs() as f64);
        }
    }
    let ms = worst / 48.0;
    assert!(ms < 15.0, "расхождение дошло до {ms:.1} мс");
}

#[test]
fn resampler_correction_changes_length() {
    let mut r = Resampler::new(48_000);
    r.set_correction(0.001);
    let mut out = Vec::new();
    for _ in 0..1000 {
        r.process(&vec![(0.5, 0.5); 480], &mut out);
    }
    let frames = (out.len() / 2) as i64;
    assert!((479_520 - 5..=479_520 + 5).contains(&frames), "кадров {frames}");
}

#[test]
fn sane_time_rejects_nonsense() {
    let now = 100_000_000;
    assert_eq!(sane_time(now - 200_000, 480, 48_000, now), now - 200_000, "нормальная метка");
    assert_eq!(sane_time(0, 480, 48_000, now), now - 100_000, "нет метки");
    assert_eq!(sane_time(now + 10_000_000, 480, 48_000, now), now - 100_000, "из будущего");
    assert_eq!(sane_time(now - 50_000_000, 480, 48_000, now), now - 100_000, "на 5 с в прошлом");
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

/// Диагностика: насколько часы микрофона расходятся с часами системы и что с этим
/// делала старая расстановка кусков. Запуск: cargo test --release mic_clock_drift -- --ignored --nocapture
#[test]
#[ignore]
fn mic_clock_drift() {
    let secs: u64 = std::env::var("CLIPBACK_TEST_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(90);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let o = open(&SourceKind::Mic(None), &en).unwrap();
        let cap: IAudioCaptureClient = o.client.GetService().unwrap();
        o.client.Start().unwrap();
        let t0 = qpc_now();
        let begin = Instant::now();
        let (mut first_qpc, mut frames_total, mut silent_pk, mut pk, mut maxabs) = (0i64, 0i64, 0, 0, 0f32);
        let mut worst_lag = 0i64; // насколько метка первого кадра отстаёт от «сейчас», мс
        // старая логика: np, допуск 10 мс, «сдвиг» np при выбрасывании
        let mut np: Option<i64> = None;
        let (mut old_sent, mut old_dropped) = (0i64, 0i64);
        // новая логика
        let (mut placer, mut rs, mut v) = (Placer::new(), Resampler::new(o.rate), Vec::new());
        let (mut new_sent, mut new_gaps) = (0i64, 0i64);
        let mut next_report = 10;
        while begin.elapsed() < Duration::from_secs(secs) {
            let n = cap.GetNextPacketSize().unwrap();
            if n == 0 {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let mut data = std::ptr::null_mut();
            let (mut frames, mut fl, mut qpc) = (0u32, 0u32, 0u64);
            cap.GetBuffer(&mut data, &mut frames, &mut fl, None, Some(&mut qpc)).unwrap();
            if fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                silent_pk += 1;
            } else if !data.is_null() {
                let bytes = std::slice::from_raw_parts(data, frames as usize * o.block);
                for f in bytes.chunks_exact(o.block) {
                    let (l, r) = frame_to_stereo(o.fmt, f, o.ch);
                    maxabs = maxabs.max(l.abs()).max(r.abs());
                }
            }
            cap.ReleaseBuffer(frames).unwrap();
            pk += 1;
            let now = qpc_now();
            worst_lag = worst_lag.max((now - qpc as i64) / 10_000);
            if first_qpc == 0 {
                first_qpc = qpc as i64;
            }
            // старая расстановка
            let f48 = frames as i64 * RATE as i64 / o.rate as i64;
            let start = pos_of(qpc as i64, t0);
            let p = np.get_or_insert(start);
            let old_align = |next: i64, start: i64, tol: i64| {
                let d = start - next;
                if d > tol { (d, 0) } else if d < -tol { (0, -d) } else { (0, 0) }
            };
            let (sil, drop) = old_align(*p, start, RATE as i64 / 100);
            *p += sil;
            let d = drop.min(f48);
            *p += d;
            old_dropped += d;
            old_sent += f48 - d;
            *p += f48 - d;
            rs.set_correction(placer.correction());
            let mut s48 = Vec::new();
            rs.process(&vec![(0.1, 0.1); frames as usize], &mut s48);
            v.clear();
            placer.place(start, s48, &mut v);
            if v.len() == 2 {
                new_gaps += v[0].1.len() as i64 / 2;
            }
            new_sent += v.last().map_or(0, |c| c.1.len() as i64 / 2);
            frames_total += frames as i64;
            if begin.elapsed().as_secs() >= next_report {
                next_report += 10;
                let by_clock = (qpc as i64 - first_qpc) as f64 * o.rate as f64 / 1e7;
                let counted = (frames_total - frames as i64) as f64;
                let ppm = (counted - by_clock) / by_clock.max(1.0) * 1e6;
                println!(
                    "{:>4} с: расхождение {:+.1} мс ({:+.0} ppm), пакетов {pk}, «тихих» {silent_pk}, пик {maxabs:.5}, задержка до {worst_lag} мс | старая логика: отдано {:.1} с, выброшено {:.1} с | новая: отдано {:.1} с, тишины вставлено {:.2} с, поправка {:+.0} ppm",
                    begin.elapsed().as_secs(),
                    (counted - by_clock) / o.rate as f64 * 1000.0,
                    ppm,
                    old_sent as f64 / RATE as f64,
                    old_dropped as f64 / RATE as f64,
                    new_sent as f64 / RATE as f64,
                    new_gaps as f64 / RATE as f64,
                    placer.correction() * 1e6
                );
            }
        }
        let _ = o.client.Stop();
    }
}
