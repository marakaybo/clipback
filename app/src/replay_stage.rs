use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::{mpsc, OnceLock},
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager};
use windows::{
    core::HSTRING,
    Win32::{Foundation::HANDLE, Storage::FileSystem::*, System::Com::CoCreateGuid},
};

static ROOT: OnceLock<PathBuf> = OnceLock::new();
static SIGNAL: OnceLock<mpsc::SyncSender<()>> = OnceLock::new();

fn session() -> Result<&'static str> {
    static ID: OnceLock<String> = OnceLock::new();
    if ID.get().is_none() {
        let _ = ID.set(unique()?);
    }
    Ok(ID.get().unwrap())
}

fn stage_handle(path: &Path) -> Result<File> {
    let dir = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    ensure!(
        dir.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "каталог задания заменён"
    );
    Ok(dir)
}

fn unique() -> Result<String> {
    Ok(format!("{:032x}", unsafe { CoCreateGuid()? }.to_u128()))
}

#[derive(Serialize, Deserialize)]
struct Job {
    session: String,
    stage_id: [u64; 3],
    stage: PathBuf,
    dest: PathBuf,
    copy: Option<CopyState>,
    published: bool,
}

#[derive(Serialize, Deserialize)]
struct CopyState {
    name: String,
    temporary: String,
    source_id: [u64; 3],
    copy_id: [u64; 3],
    target: String,
    ready: bool,
}

// Include creation time because Windows can recycle a deleted file's index.
fn identity(file: &File) -> Result<[u64; 3]> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)? };
    let pair = |high, low| (u64::from(high) << 32) | u64::from(low);
    Ok([
        info.dwVolumeSerialNumber.into(),
        pair(info.nFileIndexHigh, info.nFileIndexLow),
        pair(
            info.ftCreationTime.dwHighDateTime,
            info.ftCreationTime.dwLowDateTime,
        ),
    ])
}

fn move_file(from: &Path, to: &Path, flags: MOVE_FILE_FLAGS) -> Result<()> {
    unsafe {
        MoveFileExW(
            &HSTRING::from(from.as_os_str()),
            &HSTRING::from(to.as_os_str()),
            flags | MOVEFILE_WRITE_THROUGH,
        )?
    };
    Ok(())
}

fn persist(path: &Path, job: &Job, replace: bool) -> Result<()> {
    let tmp = path.with_extension(format!("{}.new", unique()?));
    let mut file = File::create_new(&tmp)?;
    serde_json::to_writer(&mut file, job)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    move_file(
        &tmp,
        path,
        if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            MOVE_FILE_FLAGS(0)
        },
    )
}

pub fn prepare(stage_root: &Path, dest_dir: &Path) -> Result<PathBuf> {
    Ok(prepare_in(
        ROOT.get().context("очередь повторов ещё не запущена")?,
        stage_root,
        dest_dir,
    )?
    .0)
}

/// Isolated queue for tests/benchmarks. Returns (writer directory, manifest).
pub(crate) fn prepare_in(
    queue: &Path,
    stage_root: &Path,
    dest_dir: &Path,
) -> Result<(PathBuf, PathBuf)> {
    ensure!(
        stage_root.is_absolute() && dest_dir.is_absolute(),
        "нужны абсолютные пути"
    );
    fs::create_dir_all(queue)?;
    fs::create_dir_all(stage_root)?;
    let id = unique()?;
    let stage = stage_root.join(format!("clipback-replay-{id}"));
    fs::create_dir(&stage)?;
    let manifest = queue.join(format!("{id}.json"));
    persist(
        &manifest,
        &Job {
            stage: stage.clone(),
            stage_id: identity(&stage_handle(&stage)?)?,
            session: session()?.into(),
            dest: dest_dir.into(),
            copy: None,
            published: false,
        },
        false,
    )?;
    Ok((stage, manifest))
}

fn plain_name(name: &str) -> bool {
    Path::new(name).file_name() == Some(name.as_ref()) && !name.contains(':')
}

fn finalized(dir: &Path) -> Result<Option<String>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path.extension().is_some_and(|ext| ext == "mp4")
            && !path.to_string_lossy().ends_with(".1t.mp4")
        {
            return Ok(Some(
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("неверное имя клипа"))?,
            ));
        }
    }
    Ok(None)
}

fn open_source(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
}

fn regular(file: &File) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "ожидался обычный файл"
    );
    Ok(())
}

// Called by a single consumer. Notify after the archive is durable, before cleanup.
pub(crate) fn process_one(manifest: &Path, notify: impl FnOnce(&Path)) -> Result<()> {
    let mut job: Job = serde_json::from_reader(File::open(manifest)?)?;
    let id = manifest
        .file_stem()
        .and_then(|s| s.to_str())
        .context("неверный манифест")?;
    ensure!(
        id.len() == 32
            && id.bytes().all(|b| b.is_ascii_hexdigit())
            && job.stage.ends_with(format!("clipback-replay-{id}"))
            && job.stage.is_absolute()
            && job.dest.is_absolute(),
        "неверный каталог задания"
    );
    let dir = match stage_handle(&job.stage) {
        Ok(dir) => Some(dir),
        Err(e)
            if job.published
                && !job.stage.try_exists()?
                && job.stage.parent().is_some_and(Path::is_dir) =>
        {
            drop(e);
            None
        }
        Err(e) => return Err(e),
    };
    if let Some(dir) = &dir {
        ensure!(identity(dir)? == job.stage_id, "каталог задания заменён");
    }
    if job.copy.is_none() {
        let Some(name) = finalized(&job.stage)? else {
            if job.session != session()? {
                // The single-instance app owns this exact per-job directory.
                // Its replay writer creates at most one .part. Leave other entries alone.
                let entries = fs::read_dir(&job.stage)?.collect::<io::Result<Vec<_>>>()?;
                if let [entry] = entries.as_slice() {
                    if entry.file_type()?.is_file()
                        && entry.path().extension().is_some_and(|e| e == "part")
                    {
                        fs::remove_file(entry.path())?;
                    }
                }
                drop(dir);
                retire(manifest, &job.stage)?;
            } else {
                ensure!(
                    manifest
                        .metadata()?
                        .modified()?
                        .elapsed()
                        .unwrap_or_default()
                        < Duration::from_secs(600),
                    "ожидание готового MP4 дольше 10 минут"
                );
            }
            return Ok(());
        };
        ensure!(plain_name(&name), "неверное имя");
        let source = open_source(&job.stage.join(&name))?;
        regular(&source)?;
        fs::create_dir_all(&job.dest)?;
        let temporary = format!(".clipback-{id}-{}.partial-copy", unique()?);
        let temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(FILE_ATTRIBUTE_HIDDEN.0)
            .open(job.dest.join(&temporary))?;
        job.copy = Some(CopyState {
            target: name.clone(),
            name,
            temporary,
            source_id: identity(&source)?,
            copy_id: identity(&temp)?,
            ready: false,
        });
        persist(manifest, &job, true)?;
    }
    let copy = job.copy.as_ref().unwrap();
    ensure!(
        plain_name(&copy.name)
            && plain_name(&copy.target)
            && plain_name(&copy.temporary)
            && copy.temporary.starts_with(&format!(".clipback-{id}-"))
            && copy.temporary.ends_with(".partial-copy"),
        "неверное имя в очереди"
    );
    let source_path = job.stage.join(&copy.name);
    let temp_path = job.dest.join(&copy.temporary);
    if !job.published {
        let mut source = open_source(&source_path)?;
        regular(&source)?;
        ensure!(
            identity(&source)? == copy.source_id,
            "исходный файл заменён"
        );
        if temp_path.try_exists()? {
            let mut temp = OpenOptions::new()
                .write(true)
                .share_mode(0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
                .open(&temp_path)?;
            regular(&temp)?;
            ensure!(identity(&temp)? == copy.copy_id, "временный файл заменён");
            if !copy.ready {
                temp.set_len(0)?;
                let size = source.metadata()?.len();
                ensure!(size > 0, "пустой клип");
                ensure!(
                    crate::winutil::free_space(&job.dest).context("не удалось проверить место")?
                        >= size,
                    "недостаточно места"
                );
                ensure!(io::copy(&mut source, &mut temp)? == size, "неполная копия");
                temp.sync_all()?;
                job.copy.as_mut().unwrap().ready = true;
                persist(manifest, &job, true)?;
            }
            drop(temp);
            loop {
                let target = job.dest.join(&job.copy.as_ref().unwrap().target);
                match move_file(&temp_path, &target, MOVE_FILE_FLAGS(0)) {
                    Ok(()) => break,
                    Err(_) if target.try_exists()? => {
                        let copy = job.copy.as_mut().unwrap();
                        let stem = Path::new(&copy.name).file_stem().unwrap().to_string_lossy();
                        copy.target = format!(
                            "{}-{}.mp4",
                            stem.chars().take(80).collect::<String>(),
                            unique()?
                        );
                        persist(manifest, &job, true)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        // A crash after the rename leaves the receipt and the published file.
        let copy = job.copy.as_ref().unwrap();
        let target = job.dest.join(&copy.target);
        let archived = OpenOptions::new()
            .access_mode(FILE_GENERIC_READ.0 | FILE_WRITE_ATTRIBUTES.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&target)?;
        regular(&archived)?;
        ensure!(
            copy.ready
                && identity(&archived)? == copy.copy_id
                && archived.metadata()?.len() == source.metadata()?.len(),
            "не удалось подтвердить перенос"
        );
        let attrs = FILE_BASIC_INFO {
            FileAttributes: FILE_ATTRIBUTE_NORMAL.0,
            ..Default::default()
        };
        unsafe {
            SetFileInformationByHandle(
                HANDLE(archived.as_raw_handle()),
                FileBasicInfo,
                (&attrs as *const FILE_BASIC_INFO).cast(),
                std::mem::size_of_val(&attrs) as u32,
            )?
        };
        job.published = true;
        persist(manifest, &job, true)?;
    }
    let copy = job.copy.as_ref().unwrap();
    let target = job.dest.join(&copy.target);
    if source_path.try_exists()? {
        // Hold the verified archive against replacement while unlinking the exact source handle.
        let archive = open_source(&target)?;
        regular(&archive)?;
        let source = OpenOptions::new()
            .access_mode(FILE_GENERIC_READ.0 | DELETE.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&source_path)?;
        regular(&source)?;
        ensure!(
            identity(&archive)? == copy.copy_id
                && identity(&source)? == copy.source_id
                && archive.metadata()?.len() == source.metadata()?.len(),
            "файлы задания изменились"
        );
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            SetFileInformationByHandle(
                HANDLE(source.as_raw_handle()),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of_val(&disposition) as u32,
            )?
        };
    }
    drop(dir);
    retire(manifest, &job.stage)?;
    notify(&target);
    Ok(())
}

fn retire(manifest: &Path, stage: &Path) -> Result<()> {
    // remove_dir only succeeds for an empty directory.
    match fs::remove_dir(stage) {
        Ok(()) => (),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            ()
        }
        Err(e) => return Err(e.into()),
    }
    fs::remove_file(manifest)?;
    Ok(())
}

pub fn wake() {
    if let Some(tx) = SIGNAL.get() {
        let _ = tx.try_send(());
    }
}

pub fn start(app: AppHandle) {
    let root = match app.path().app_local_data_dir() {
        Ok(dir) => dir.join("replay-queue"),
        Err(e) => {
            crate::show_toast(&app, "Очередь повторов недоступна", &e.to_string(), "error");
            return;
        }
    };
    let _ = ROOT.set(root.clone());
    SIGNAL.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            use windows::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            };
            if let Err(e) =
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }
            {
                crate::log(format!("background I/O priority: {e}"));
            }
            let mut previous = std::collections::HashSet::new();
            loop {
                let mut current = std::collections::HashSet::new();
                let mut report = |error: String| {
                    if current.insert(error.clone()) && !previous.contains(&error) {
                        crate::log(&error);
                        crate::show_toast(&app, "Перенос повтора отложен", &error, "error");
                    }
                };
                let scan = || -> Result<_> {
                    fs::create_dir_all(&root)?;
                    Ok(fs::read_dir(&root)?)
                };
                match scan() {
                    Err(e) => report(format!("{e:#}")),
                    Ok(entries) => {
                        for entry in entries {
                            let path = match entry {
                                Ok(entry) => entry.path(),
                                Err(e) => {
                                    report(e.to_string());
                                    continue;
                                }
                            };
                            if path.extension().is_some_and(|ext| ext == "json") {
                                if let Err(e) = process_one(&path, |target| {
                                    crate::clips_changed(&app);
                                    let _ = app.emit("clip-saved", target.to_string_lossy());
                                    crate::auto_clean_later(&app);
                                }) {
                                    report(format!("{e:#} ({})", path.display()));
                                }
                            }
                        }
                    }
                }
                previous = current;
                let _ = rx.recv_timeout(Duration::from_secs(30));
            }
        });
        tx
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_queue_restart_collision_and_retry() -> Result<()> {
        let root = std::env::temp_dir().join(unique()?);
        let queue = root.join("queue");
        let dest = root.join("archive");
        let (first, m1) = prepare_in(&queue, &root.join("stage-a"), &dest)?;
        let (second, m2) = prepare_in(&queue, &root.join("stage-b"), &dest)?;
        assert!(!dest.exists());
        fs::write(first.join("video.part"), b"partial")?;
        process_one(&m1, |_| panic!("unfinished"))?;
        fs::write(first.join("video.mp4"), b"one")?;
        fs::write(second.join("video.mp4"), b"two")?;
        fs::write(&dest, b"offline")?;
        assert!(process_one(&m1, |_| panic!()).is_err());
        assert!(first.join("video.mp4").exists());
        fs::remove_file(&dest)?;
        process_one(&m2, |p| assert_eq!(fs::read(p).unwrap(), b"two"))?;
        process_one(&m1, |p| assert_eq!(fs::read(p).unwrap(), b"one"))?;
        assert_eq!(fs::read(dest.join("video.mp4"))?, b"two");
        assert!(first.join("video.part").exists());
        assert_eq!(fs::read_dir(queue)?.count(), 0);
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn replay_queue_interrupted_job_cleanup() -> Result<()> {
        let root = std::env::temp_dir().join(unique()?);
        for partial in [false, true] {
            let (stage, manifest) = prepare_in(
                &root.join("q"),
                &root.join("staging"),
                &root.join("archive"),
            )?;
            let mut job: Job = serde_json::from_reader(File::open(&manifest)?)?;
            job.session = "previous process".into();
            persist(&manifest, &job, true)?;
            if partial {
                fs::write(stage.join("video.part"), b"incomplete")?;
            }
            process_one(&manifest, |_| panic!("unfinished job"))?;
            assert!(!stage.exists() && !manifest.exists());
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn replay_queue_recovers_rename_and_preserves_source_if_archive_offline() -> Result<()> {
        let root = std::env::temp_dir().join(unique()?);
        let dest = root.join("archive");
        let (stage, manifest) = prepare_in(&root.join("q"), &root.join("staging"), &dest)?;
        let name = "video.mp4";
        let source = stage.join(name);
        fs::create_dir(&dest)?;
        fs::write(&source, b"clip")?;
        let target = dest.join(name);
        fs::copy(&source, &target)?;
        let mut job: Job = serde_json::from_reader(File::open(&manifest)?)?;
        job.copy = Some(CopyState {
            name: name.into(),
            target: name.into(),
            source_id: identity(&File::open(&source)?)?,
            copy_id: identity(&File::open(&target)?)?,
            ready: true,
            temporary: format!(
                ".clipback-{}-test.partial-copy",
                manifest.file_stem().unwrap().to_string_lossy()
            ),
        });
        job.published = true;
        persist(&manifest, &job, true)?;
        fs::rename(&dest, root.join("offline"))?;
        assert!(process_one(&manifest, |_| panic!()).is_err());
        assert!(source.exists() && manifest.exists());
        fs::rename(root.join("offline"), &dest)?;
        // Model the earlier crash window just after MoveFileExW succeeded.
        job.published = false;
        persist(&manifest, &job, true)?;
        process_one(&manifest, |p| assert_eq!(p, target))?;
        assert!(!source.exists() && !manifest.exists());
        assert_eq!(fs::read_dir(&dest)?.count(), 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
