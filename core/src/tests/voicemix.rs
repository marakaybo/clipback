use super::*;

const SEC: usize = RATE as usize;

/// Стерео-синус амплитуды `a`, `secs` секунд.
fn tone(a: f32, hz: f32, secs: f32) -> Vec<f32> {
    let n = (secs * SEC as f32) as usize;
    (0..n).flat_map(|i| {
        let x = a * (i as f32 * hz * std::f32::consts::TAU / SEC as f32).sin();
        [x, x]
    }).collect()
}

/// «Игра»: шумоподобный сигнал с пиками около `a`.
fn noise(a: f32, secs: f32) -> Vec<f32> {
    let mut s = 12345u32;
    let n = (secs * SEC as f32) as usize;
    (0..n).flat_map(|_| {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        let x = a * ((s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0);
        [x, x]
    }).collect()
}

fn peak(v: &[f32]) -> f32 {
    v.iter().fold(0.0, |a, &b| a.max(b.abs()))
}

#[test]
fn quiet_voice_is_raised_toward_the_game() {
    // голос на 12 дБ тише игры — как в клипах из Discord-комнаты
    let mut vm = VoiceMix::new();
    let (game, voice) = (noise(0.4, 8.0), tone(0.1, 220.0, 8.0));
    vm.process(&game, &voice, 1.0, 1.0);
    assert!((3.6..=5.0).contains(&vm.boost()), "голос подтянут почти до игры: {}", vm.boost());
    assert_eq!(vm.sys_boost(), 1.0, "игру при тихом голосе не трогаем");
    // голос без игры после выравнивания звучит в ~4 раза громче исходного
    let only_voice = vm.process(&[], &tone(0.1, 220.0, 0.5), 1.0, 1.0);
    assert!(peak(&only_voice) > 0.33, "{}", peak(&only_voice));
}

#[test]
fn boost_is_limited_both_ways() {
    let mut vm = VoiceMix::new();
    vm.process(&noise(0.8, 10.0), &tone(0.02, 220.0, 10.0), 1.0, 1.0);
    assert!(vm.boost() <= VoiceMix::MAX_BOOST + 1e-3 && vm.boost() > 5.0, "{}", vm.boost());
    let mut vm = VoiceMix::new();
    vm.process(&noise(0.05, 10.0), &tone(0.8, 220.0, 10.0), 1.0, 1.0);
    assert!((vm.boost() - VoiceMix::MIN_BOOST).abs() < 0.05, "громкий голос приглушён не больше чем на 6 дБ: {}", vm.boost());
    assert!((vm.sys_boost() - VoiceMix::MAX_SYS).abs() < 0.1, "остальное добрала игра: {}", vm.sys_boost());
}

#[test]
fn pauses_and_silent_game_keep_the_gain() {
    let mut vm = VoiceMix::new();
    vm.process(&noise(0.4, 12.0), &tone(0.1, 220.0, 12.0), 1.0, 1.0);
    let b = vm.boost();
    // голос замолчал, игра идёт — усиление не меняется (нет «дыхания» шума в паузах)
    vm.process(&noise(0.4, 3.0), &[], 1.0, 1.0);
    assert!((vm.boost() - b).abs() < 1e-4);
    // игра стихла, голос есть — к единице не сползает
    vm.process(&[], &tone(0.1, 220.0, 3.0), 1.0, 1.0);
    assert!((vm.boost() - b).abs() < 0.05 * b, "{b} → {}", vm.boost());
    // без игры с самого начала голос не трогаем
    let mut vm = VoiceMix::new();
    vm.process(&[], &tone(0.1, 220.0, 3.0), 1.0, 1.0);
    assert_eq!(vm.boost(), 1.0);
}

#[test]
fn turn_taking_is_leveled_too() {
    // Discord-комната: друзья (система) и вы (микрофон) говорят по очереди, вы на 12 дБ тише
    let mut vm = VoiceMix::new();
    for _ in 0..4 {
        vm.process(&noise(0.4, 2.0), &[], 1.0, 1.0);
        vm.process(&[], &tone(0.1, 220.0, 2.0), 1.0, 1.0);
    }
    assert!(vm.boost() > 2.5, "{}", vm.boost());
    // и наоборот: вы намного громче друзей — голос слегка приглушается
    let mut vm = VoiceMix::new();
    for _ in 0..4 {
        vm.process(&noise(0.05, 2.0), &[], 1.0, 1.0);
        vm.process(&[], &tone(0.6, 220.0, 2.0), 1.0, 1.0);
    }
    assert!(vm.boost() < 0.6 && vm.sys_boost() > 2.0, "{} {}", vm.boost(), vm.sys_boost());
}

#[test]
fn loud_sum_never_clips() {
    let mut vm = VoiceMix::new();
    let out = vm.process(&noise(0.95, 3.0), &tone(0.9, 300.0, 3.0), 2.0, 2.0);
    assert!(peak(&out) <= 0.9 + 1e-4, "ограничитель держит потолок: {}", peak(&out));
    assert!(out.iter().all(|x| x.is_finite()));
}

#[test]
fn user_volumes_apply_after_leveling() {
    let (game, voice) = (noise(0.4, 4.0), tone(0.1, 220.0, 4.0));
    let mut a = VoiceMix::new();
    let full = a.process(&game, &voice, 1.0, 1.0);
    let mut b = VoiceMix::new();
    let muted = b.process(&game, &voice, 1.0, 0.0);
    assert!((a.boost() - b.boost()).abs() < 1e-4, "выравнивание считается по исходному звуку");
    // микрофон 0 % — в сумме только игра (без ограничения: пики игры ниже потолка)
    let diff = muted.iter().zip(&game).fold(0f32, |m, (x, g)| m.max((x - g).abs()));
    assert!(diff < 1e-4, "{diff}");
    assert_ne!(full, muted);
}

#[test]
fn shorter_input_is_padded_with_silence() {
    let mut vm = VoiceMix::new();
    let out = vm.process(&noise(0.3, 1.0), &tone(0.1, 220.0, 0.5), 1.0, 1.0);
    assert_eq!(out.len(), SEC * 2);
}

#[test]
fn primed_mix_is_leveled_from_the_start() {
    let (game, voice) = (noise(0.4, 6.0), tone(0.1, 220.0, 6.0));
    let vm = VoiceMix::primed(&game, &voice);
    assert!((3.6..=5.0).contains(&vm.boost()), "{}", vm.boost());
    // первая секунда уже с поднятым голосом — как и последняя
    let mut a = VoiceMix::primed(&game, &voice);
    let out = a.process(&[], &voice[..SEC * 2], 1.0, 1.0);
    assert!(peak(&out[SEC / 2..]) > 0.33, "{}", peak(&out));
}
