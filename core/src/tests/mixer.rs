use super::*;

fn chunk(source: usize, pos: i64, frames: usize, v: f32) -> Chunk {
    Chunk { source, pos, samples: vec![v; frames * 2] }
}

#[test]
fn layout_simple_and_advanced() {
    let names = |c: &AudioConfig| track_names(c);
    assert_eq!(names(&AudioConfig::default()), vec!["Звук системы"]);
    let both = AudioConfig { mic: true, ..Default::default() };
    assert_eq!(names(&both), vec!["Всё вместе", "Звук системы", "Микрофон"]);
    let adv = AudioConfig {
        mic: true,
        apps: vec![AppTrack { name: "Discord".into(), exe: "Discord.exe".into() }, AppTrack { name: " ".into(), exe: "bf6.exe".into() }],
        ..Default::default()
    };
    assert_eq!(names(&adv), vec!["Всё вместе", "Звук системы", "Микрофон", "Discord", "bf6.exe"]);
    let l = layout(&adv);
    assert_eq!(l.sources.len(), 4);
    assert_eq!(l.tracks[0].sources, vec![0, 1], "«всё вместе» = система + микрофон, без приложений (они уже в системе)");
    assert!(matches!(l.sources[3].kind, SourceKind::App(ref e) if e == "bf6.exe"));
    let none = AudioConfig { system: false, ..Default::default() };
    assert!(names(&none).is_empty());
}

#[test]
fn late_microphone_is_not_lost() {
    // система присылает звук сразу на 200 мс вперёд, микрофон — маленькими кусками и с опозданием
    let t = Instant::now();
    let mut st = MixState::new(2, t);
    st.push(chunk(0, 0, 9600, 0.1), t);
    st.push(chunk(1, 0, 480, 0.5), t);
    let mut got = Vec::new();
    while let Some(b) = st.next_block(t + Duration::from_millis(100)) {
        got.push(b);
    }
    // микрофон отстал на 400 мс по времени прихода — но он жив, его ждём
    st.push(chunk(1, 480, 9120, 0.5), t + Duration::from_millis(400));
    while let Some(b) = st.next_block(t + Duration::from_millis(420)) {
        got.push(b);
    }
    let mic: Vec<f32> = got.iter().flat_map(|(_, p)| p[1].iter().copied()).collect();
    assert_eq!(mic.len(), 9600 * 2, "весь звук микрофона дошёл");
    assert!(mic.iter().all(|&v| v == 0.5), "в звуке микрофона нет дыр");
}

#[test]
fn silent_source_does_not_block_others() {
    let t = Instant::now();
    let mut st = MixState::new(2, t);
    st.push(chunk(0, 0, 480, 0.1), t);
    st.push(chunk(1, 0, 480, 0.2), t);
    assert!(st.next_block(t).is_some());
    // второй источник замолчал (например, выдернули микрофон)
    let later = t + Duration::from_secs(1);
    st.push(chunk(0, 480, 4800, 0.1), later);
    let (_, parts) = st.next_block(later).expect("первый источник не ждёт пропавший");
    assert!(parts[1].iter().all(|&v| v == 0.0), "вместо пропавшего — тишина");
    assert!(parts[0].iter().all(|&v| v == 0.1));
}

#[test]
fn waits_for_all_sources_at_start() {
    let t = Instant::now();
    let mut st = MixState::new(2, t);
    st.push(chunk(0, 0, 4800, 0.1), t);
    assert!(st.next_block(t).is_none(), "второй источник ещё не начал");
    let later = t + Duration::from_millis(1100);
    st.push(chunk(0, 4800, 4800, 0.1), later);
    assert!(st.next_block(later).is_some(), "через секунду пишем без него");
}

#[test]
fn mic_gain_applies_and_clips_softly() {
    let sources = vec![SourceDef { kind: SourceKind::System, gain: 1.0, agc: false }, SourceDef { kind: SourceKind::Mic(None), gain: 4.0, agc: false }];
    let parts = vec![vec![0.1; 4], vec![0.1; 4]];
    let mic = render_track(&TrackDef { name: "м".into(), sources: vec![1] }, &sources, &parts);
    assert!((mic[0] - 0.4).abs() < 1e-6);
    let mix = render_track(&TrackDef { name: "в".into(), sources: vec![0, 1] }, &sources, &parts);
    assert!((mix[0] - 0.5).abs() < 1e-6);
    let loud = render_track(&TrackDef { name: "м".into(), sources: vec![1] }, &sources, &vec![vec![0.0; 2], vec![0.9; 2]]);
    assert!(loud[0] <= 1.0);
}

#[test]
fn queue_fills_gaps_and_takes_exactly() {
    let mut q = Queue::new();
    q.push(chunk(0, 100, 10, 1.0));
    q.push(chunk(0, 115, 5, 2.0)); // пропуск 110..115
    assert_eq!(q.end(), Some(120));
    let t = q.take(95, 10);
    assert_eq!(&t[..10], &[0.0; 10]);
    assert_eq!(&t[10..], &[1.0; 10]);
    let t = q.take(105, 15);
    assert_eq!(&t[..10], &[1.0; 10]);
    assert_eq!(&t[10..20], &[0.0; 10]);
    assert_eq!(&t[20..], &[2.0; 10]);
    q.push(chunk(0, 118, 3, 3.0)); // наложение отбрасывается
    assert_eq!(q.end(), Some(121));
}

#[test]
fn soft_clip_is_transparent_below_threshold() {
    assert_eq!(soft_clip(0.5), 0.5);
    assert_eq!(soft_clip(-0.9), -0.9);
    assert!(soft_clip(3.0) <= 1.0 && soft_clip(3.0) > 0.95);
}

#[test]
fn agc_lifts_quiet_voice_but_not_silence() {
    // «голос» на −50 дБ: после пары секунд должен стать громким
    let mut agc = Agc::new();
    let mut quiet: Vec<f32> = (0..RATE as usize * 2 * 3).map(|i| ((i as f32) * 0.05).sin() * 0.003).collect();
    agc.process(&mut quiet);
    let tail = &quiet[quiet.len() - 9600..];
    let peak = tail.iter().fold(0f32, |a, &b| a.max(b.abs()));
    assert!(peak > 0.12, "голос остался тихим: {peak}");

    // шум микрофона на −100 дБ не раздувается
    let mut agc = Agc::new();
    let mut noise: Vec<f32> = (0..RATE as usize * 2 * 3).map(|i| ((i as f32) * 0.7).sin() * 0.00001).collect();
    agc.process(&mut noise);
    let peak = noise.iter().fold(0f32, |a, &b| a.max(b.abs()));
    assert!(peak < 0.0001, "шум раздуло: {peak}");

    // громкий голос не перегружается
    let mut agc = Agc::new();
    let mut loud: Vec<f32> = (0..RATE as usize * 2).map(|i| ((i as f32) * 0.05).sin() * 0.95).collect();
    agc.process(&mut loud);
    assert!(loud.iter().all(|x| x.abs() <= 1.0));
}
