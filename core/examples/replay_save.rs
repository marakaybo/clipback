use anyhow::{ensure, Result};
use clipback_core::{aac::AudioPacket, encoder::Packet, mux};
use mp4::{Mp4Reader, TrackType};
use std::{fs::File, io::BufReader, path::PathBuf, sync::Arc, time::Instant};

/// Time the recorder's file writer with real encoded packets already resident in memory.
/// Arguments: source Clipback replay, fresh output path, number of writes.
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    ensure!(args.len() == 4, "usage: replay_save INPUT OUTPUT RUNS");
    let output = PathBuf::from(&args[2]);
    ensure!(!output.exists() && !output.with_extension("part").exists(), "output already exists");
    let runs: usize = args[3].to_string_lossy().parse()?;
    let file = File::open(&args[1])?;
    let size = file.metadata()?.len();
    let mut reader = Mp4Reader::read_header(BufReader::new(file), size)?;
    let mut ids: Vec<_> = reader.tracks().keys().copied().collect();
    ids.sort_unstable();
    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut dimensions = (0, 0);
    for id in ids {
        let track = &reader.tracks()[&id];
        let count = track.sample_count();
        let scale = u64::from(track.timescale());
        match track.track_type()? {
            TrackType::Video => {
                dimensions = (track.width().into(), track.height().into());
                let mut header = Vec::new();
                for nal in [track.sequence_parameter_set()?, track.picture_parameter_set()?] {
                    header.extend_from_slice(&[0, 0, 0, 1]);
                    header.extend_from_slice(nal);
                }
                for index in 1..=count {
                    let sample = reader.read_sample(id, index)?.unwrap();
                    let mut data = if sample.is_sync { header.clone() } else { Vec::new() };
                    let mut bytes = sample.bytes.as_ref();
                    while !bytes.is_empty() {
                        let len = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
                        data.extend_from_slice(&[0, 0, 0, 1]);
                        data.extend_from_slice(&bytes[4..4 + len]);
                        bytes = &bytes[4 + len..];
                    }
                    video.push(Arc::new(Packet { data, pts: (sample.start_time * 10_000_000 / scale) as i64, key: sample.is_sync }));
                }
            }
            TrackType::Audio => {
                let mut packets = Vec::new();
                for index in 1..=count {
                    let sample = reader.read_sample(id, index)?.unwrap();
                    packets.push(Arc::new(AudioPacket { pos: (sample.start_time * 48_000 / scale) as i64, data: sample.bytes.to_vec() }));
                }
                audio.push(packets);
            }
            _ => {}
        }
    }
    let inputs: Vec<_> = audio.iter().map(|packets| mux::AudioInput { packets, silence: &packets[0].data }).collect();
    let names = mux::default_track_names(inputs.len());
    for run in 0..runs {
        let start = Instant::now();
        mux::write_av_named(&output, &video, &inputs, &names, dimensions.0, dimensions.1)?;
        println!("write_ms={}", start.elapsed().as_millis());
        if run + 1 < runs { std::fs::remove_file(&output)?; }
    }
    Ok(())
}
