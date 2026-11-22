//! Optional real-drive comparison. Builds on the core replay_save example.
//! Set CLIPBACK_BENCH_WRITER, CLIPBACK_BENCH_INPUT, CLIPBACK_BENCH_STAGE and
//! CLIPBACK_BENCH_ARCHIVE, then run this ignored test with --nocapture.
use crate::replay_stage::{prepare_in, process_one};
use anyhow::{ensure, Context, Result};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

fn setting(key: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(
        env::var_os(key).with_context(|| format!("set {key}"))?,
    ))
}

fn mux(writer: &Path, input: &Path, output: &Path) -> Result<u128> {
    let result = Command::new(writer)
        .arg(input)
        .arg(output)
        .arg("1")
        .output()?;
    ensure!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(std::str::from_utf8(&result.stdout)?
        .lines()
        .find_map(|line| line.strip_prefix("write_ms="))
        .context("missing write_ms")?
        .parse()?)
}

#[test]
#[ignore = "requires an explicit input and two output drives"]
fn compare_replay_drives() -> Result<()> {
    let input = setting("CLIPBACK_BENCH_INPUT")?;
    let writer = setting("CLIPBACK_BENCH_WRITER")?;
    let name = format!(
        "clipback-measure-{}",
        unsafe { windows::Win32::System::Com::CoCreateGuid()? }.to_u128()
    );
    let stage = setting("CLIPBACK_BENCH_STAGE")?.join(&name);
    let dest = setting("CLIPBACK_BENCH_ARCHIVE")?.join(&name);
    fs::create_dir(&stage)?;
    fs::create_dir(&dest)?;
    let queue = stage.join("queue");
    let direct = dest.join("direct.mp4");
    // Alternate direction to spread read-cache and disk effects across both paths.
    let fingerprint = |file: &Path| -> Result<String> {
        let output = Command::new("certutil.exe")
            .arg("-hashfile")
            .arg(file)
            .arg("SHA256")
            .output()?;
        ensure!(output.status.success(), "hash failed");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
            .context("missing SHA-256")
    };
    let mut expected_hash = String::new();
    for run in 0..5 {
        for fast in [run % 2 == 1, run % 2 == 0] {
            if !fast {
                let elapsed = mux(&writer, &input, &direct)?;
                println!("direct_ms={elapsed}");
                if run == 0 {
                    expected_hash = fingerprint(&direct)?;
                }
                fs::remove_file(&direct)?;
                continue;
            }
            let start = Instant::now();
            let (dir, manifest) = prepare_in(&queue, &stage, &dest)?;
            let prepare_ms = start.elapsed().as_millis();
            let local = dir.join("staged.mp4");
            let saved_ms = mux(&writer, &input, &local)? + prepare_ms;
            let local_bytes = fs::metadata(&local)?.len();
            let start = Instant::now();
            let mut published = None;
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        use windows::Win32::System::Threading::{
                            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
                        };
                        unsafe {
                            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN)?
                        };
                        process_one(&manifest, |p| published = Some(p.to_owned()))
                    })
                    .join()
                    .unwrap()
            })?;
            let archive_ms = start.elapsed().as_millis();
            let target = published.context("archive missing")?;
            ensure!(!local.exists() && fs::metadata(&target)?.len() == local_bytes);
            println!("fast_ms={saved_ms} background_ms={archive_ms} bytes={local_bytes}");
            if run == 0 {
                ensure!(
                    fingerprint(&target)? == expected_hash,
                    "changed archive bytes"
                );
            }
            fs::remove_file(target)?;
        }
    }
    fs::remove_dir(queue)?;
    fs::remove_dir(stage)?;
    fs::remove_dir(dest)?;
    Ok(())
}
