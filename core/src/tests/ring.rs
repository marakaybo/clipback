use super::*;

fn pkt(i: i64, key: bool) -> Packet {
    Packet { data: vec![0u8; 100], pts: i * 10_000_000 / 10, key } // 10 к/с
}

/// 10 к/с, ключевой кадр каждые 10 кадров (раз в секунду).
fn filled(ring: &mut Ring, secs: i64) {
    for i in 0..secs * 10 {
        ring.push(pkt(i, i % 10 == 0));
    }
}

#[test]
fn ignores_frames_before_first_keyframe() {
    let mut r = Ring::new(60);
    r.push(pkt(0, false));
    r.push(pkt(1, false));
    assert_eq!(r.bytes(), 0);
    r.push(pkt(2, true));
    assert_eq!(r.bytes(), 100);
}

#[test]
fn keeps_only_last_seconds_and_starts_with_keyframe() {
    let mut r = Ring::new(5);
    filled(&mut r, 20);
    let snap = r.snapshot(None);
    assert!(snap[0].key);
    assert!(r.duration() <= 5.0 + 1e-9, "длина {}", r.duration());
    assert!(r.duration() >= 4.0, "длина {}", r.duration());
    assert_eq!(r.bytes(), snap.len() * 100);
}

#[test]
fn snapshot_of_last_seconds_starts_at_keyframe() {
    let mut r = Ring::new(60);
    filled(&mut r, 10);
    let snap = r.snapshot(Some(3));
    assert!(snap[0].key);
    let len = (snap.last().unwrap().pts - snap[0].pts) as f64 / 1e7;
    assert!((3.0..=4.0).contains(&len), "длина {len}");
}

#[test]
fn snapshot_longer_than_buffer_returns_everything() {
    let mut r = Ring::new(60);
    filled(&mut r, 3);
    assert_eq!(r.snapshot(Some(100)).len(), 30);
    assert_eq!(r.snapshot(None).len(), 30);
}

#[test]
fn shrinking_buffer_drops_old_groups() {
    let mut r = Ring::new(60);
    filled(&mut r, 10);
    r.set_length(2);
    assert!(r.duration() <= 2.0 + 1e-9);
    assert!(r.snapshot(None)[0].key);
}

#[test]
fn empty_ring() {
    let r = Ring::new(10);
    assert!(r.snapshot(None).is_empty());
    assert_eq!(r.duration(), 0.0);
}
