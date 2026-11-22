use super::{after_stall, frame_pts, frame_slot, picture_slot};
use crate::aac::{AudioPacket, FRAME};
use crate::encoder::Packet;
use crate::mux::{self, AudioInput};
use crate::ring::Ring;

#[test]
fn recording_sync_clock_after_seven_days() {
    let seconds = 7 * 24 * 3600i64;
    let audio = crate::ring::audio_time(seconds * i64::from(crate::audio::RATE));
    for fps in [10, 30, 59, 60, 120, 144, 240] {
        let slot = seconds * i64::from(fps);
        assert_eq!(frame_pts(slot, fps), audio);
        for index in slot..slot + i64::from(fps) {
            assert_eq!(frame_slot(frame_pts(index, fps), fps), index);
            assert_eq!(frame_slot(frame_pts(index + 1, fps) - 1, fps), index);
        }
    }
    // Before 0.5.7 the deadline used integer ns/frame and the PTS integer 100 ns/frame.
    let old_index = seconds * 1_000_000_000 / (1_000_000_000 / 60);
    let old_error = audio - old_index * (10_000_000 / 60);
    assert_eq!(old_error, 24_025_334);
    println!("7 days at 60 fps: video early by {:.6} s before, 0 s after", old_error as f64 / 1e7);
}

#[test]
fn recording_sync_late_present_and_repeats() {
    let fps = 60;
    let last = 600;
    let scheduled = last + 1;
    let presented = frame_slot(140_000_000, fps); // first new image at 14 s
    let old_gap = frame_pts(scheduled, fps) - frame_pts(last, fps);
    let corrected = picture_slot(scheduled, presented, fps);
    let gap = frame_pts(corrected, fps) - frame_pts(last, fps);
    assert_eq!(old_gap, 166_666);
    assert_eq!(gap, 40_000_000);
    assert_eq!(frame_pts(corrected, fps), crate::ring::audio_time(14 * 48_000));
    println!("4 s stall: previous image held {:.6} -> 4 s; new image early by {:.6} -> 0 s",
             old_gap as f64 / 1e7, (gap - old_gap) as f64 / 1e7);

    // Encoder delayed with an already-copied image, pointer-only capture,
    // and ordinary jitter must keep the scheduled duplicate/catch-up slot.
    assert_eq!(picture_slot(scheduled, last, fps), scheduled);
    assert_eq!(picture_slot(scheduled, 0, fps), scheduled);
    assert_eq!(picture_slot(scheduled, scheduled + 1 + 30, fps), scheduled);
    assert!(after_stall(scheduled, presented, fps));
}

/// Exercise the original-file writer. Packet payloads are identifiers;
/// the assertions concern the sample tables and payload placement.
#[test]
fn saved_original_preserves_audio_marker_after_capture_gap() {
    const FPS: u32 = 60;
    const RATE: i64 = 48_000;
    let seconds = 7 * 24 * 3600i64;
    let base = seconds * i64::from(FPS);
    let present = frame_slot((seconds + 8) * 10_000_000, FPS);
    let resumed = picture_slot(base + 241, present, FPS);
    let mut ring = Ring::new(30);
    ring.set_audio_tracks(1);
    for (i, slot) in [base, base + 1, base + 240, resumed, resumed + 1].into_iter().enumerate() {
        let data = if i == 0 {
            vec![0, 0, 1, 0x67, 0x64, 0, 0x28, 1, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1]
        } else {
            vec![0, 0, 1, 0x41, i as u8]
        };
        ring.push(Packet { data, pts: frame_pts(slot, FPS), key: i == 0 });
    }
    let mark = (seconds + 8) * RATE;
    ring.push_audio(0, AudioPacket { data: vec![42], pos: mark });
    let snap = ring.snapshot_all(None);
    let path = std::env::temp_dir().join(format!("clipback-recording-sync-{}.mp4", std::process::id()));
    let audio = AudioInput { packets: &snap.audio[0], silence: &[0] };
    mux::write_av(&path, &snap.video, &[audio], 640, 360).unwrap();

    let mut file = mux::open(&path).unwrap();
    let held = file.read_sample(1, 3).unwrap().unwrap();
    let picture = file.read_sample(1, 4).unwrap().unwrap();
    let sound = file.read_sample(2, (8 * RATE / FRAME + 1) as u32).unwrap().unwrap();
    assert_eq!(u64::from(held.duration), 4 * mux::TIMESCALE);
    assert_eq!(picture.start_time, 8 * mux::TIMESCALE);
    assert_eq!(sound.bytes.as_ref(), &[42]);
    assert_eq!(sound.start_time * mux::TIMESCALE, picture.start_time * RATE as u64);
    drop(file);
    std::fs::remove_file(path).unwrap();
}
