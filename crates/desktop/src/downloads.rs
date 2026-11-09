//! Pinned, opt-in model installation. The UI only reads small in-memory snapshots.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

mod speaker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Checking,
    Idle,
    Downloading,
    Stopping,
    Verifying,
    Installing,
    Ready,
    Error(String),
}
impl State {
    pub fn active(&self) -> bool {
        matches!(
            self,
            Self::Checking
                | Self::Downloading
                | Self::Stopping
                | Self::Verifying
                | Self::Installing
        )
    }
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub name: String,
    pub detail: String,
    pub received: u64,
    pub total: u64,
    pub state: State,
    pub voice_only: bool,
}
#[derive(Clone, Deserialize)]
struct Artifact {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Deserialize)]
struct Archive {
    url: String,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Deserialize)]
struct DownloadFile {
    #[serde(flatten)]
    artifact: Artifact,
    url: String,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Asr,
    Translation,
    LanguageIdentification,
    Speaker,
}
impl Role {
    fn voice_only(self) -> bool {
        matches!(self, Self::Asr | Self::Speaker)
    }
}
#[derive(Clone)]
struct Row {
    id: String,
    role: Role,
    name: String,
    detail: String,
    archive: Option<Archive>,
    files: Vec<Artifact>,
    urls: Vec<String>,
}
#[derive(Deserialize)]
struct AsrCatalog {
    id: String,
    directory: String,
    url: String,
    bytes: u64,
    sha256: String,
    artifacts: Vec<Artifact>,
}
#[derive(Deserialize)]
struct TranslationCatalog {
    id: String,
    files: Vec<DownloadFile>,
}
#[derive(Deserialize)]
struct LidCatalog {
    file: String,
    url: String,
    bytes: u64,
    sha256: String,
}
#[derive(Deserialize)]
struct Catalog {
    schema_version: u32,
    default_asr: String,
    asr: Vec<AsrCatalog>,
    translation: TranslationCatalog,
    language_identification: LidCatalog,
}
fn rows() -> Vec<Row> {
    let catalog: Catalog =
        serde_json::from_str(include_str!("../../../model-manifests/catalog.json"))
            .expect("embedded model catalog");
    assert_eq!(catalog.schema_version, 2);
    let asr = catalog
        .asr
        .into_iter()
        .find(|a| a.id == catalog.default_asr)
        .expect("default ASR catalog entry");
    let translation = catalog.translation;
    let lid = catalog.language_identification;
    vec![
        Row {
            id: asr.id,
            role: Role::Asr,
            name: "Speech recognition".into(),
            detail: "Nemotron multilingual streaming ASR".into(),
            archive: Some(Archive {
                url: asr.url,
                bytes: asr.bytes,
                sha256: asr.sha256,
            }),
            files: asr
                .artifacts
                .into_iter()
                .map(|mut a| {
                    a.path = format!("{}/{}", asr.directory, a.path);
                    a
                })
                .collect(),
            urls: Vec::new(),
        },
        Row {
            id: translation.id.clone(),
            role: Role::Translation,
            name: "Translation".into(),
            detail: "M2M100 multilingual translation".into(),
            archive: None,
            urls: translation.files.iter().map(|f| f.url.clone()).collect(),
            files: translation
                .files
                .into_iter()
                .map(|f| {
                    let mut a = f.artifact;
                    a.path = format!("{}/{}", translation.id, a.path);
                    a
                })
                .collect(),
        },
        Row {
            id: "language-identification".into(),
            role: Role::LanguageIdentification,
            name: "Language identification".into(),
            detail: "fastText language identification".into(),
            archive: None,
            urls: vec![lid.url],
            files: vec![Artifact {
                path: format!("{}/{}", translation.id, lid.file),
                bytes: lid.bytes,
                sha256: lid.sha256,
            }],
        },
        speaker::row(),
    ]
}
struct Shared {
    snapshots: Mutex<Vec<Snapshot>>,
    changed: AtomicBool,
    voice_enabled: AtomicBool,
    check_voice: AtomicUsize,
}
impl Shared {
    fn update(&self, index: usize, f: impl FnOnce(&mut Snapshot)) {
        f(&mut self.snapshots.lock().unwrap()[index]);
        self.changed.store(true, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
struct Progress<'a> {
    shared: &'a Shared,
    index: usize,
}
enum Command {
    Start,
}
pub struct Manager {
    shared: Arc<Shared>,
    controls: Vec<(crossbeam_channel::Sender<Command>, Arc<AtomicBool>)>,
}
impl Manager {
    pub fn new(
        root: PathBuf,
        voice_enabled: bool,
        shutdown: &crate::session::ShutdownWorkers,
    ) -> Self {
        let rows = rows();
        let shared = Arc::new(Shared {
            snapshots: Mutex::new(
                rows.iter()
                    .map(|r| Snapshot {
                        name: r.name.clone(),
                        detail: r.detail.clone(),
                        received: 0,
                        total: total(r),
                        state: if r.role.voice_only() && !voice_enabled {
                            State::Idle
                        } else {
                            State::Checking
                        },
                        voice_only: r.role.voice_only(),
                    })
                    .collect(),
            ),
            changed: AtomicBool::new(true),
            voice_enabled: AtomicBool::new(voice_enabled),
            check_voice: AtomicUsize::new(1),
        });
        let mut controls = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let cancel = Arc::new(AtomicBool::new(false));
            let quit = Arc::new(AtomicBool::new(false));
            let (worker_cancel, worker_quit, shared, root) =
                (cancel.clone(), quit.clone(), shared.clone(), root.clone());
            let worker = thread::spawn(move || {
                let result = (|| -> Result<()> {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    let client = reqwest::Client::builder()
                        .connect_timeout(Duration::from_secs(20))
                        .redirect(reqwest::redirect::Policy::limited(10))
                        .build()?;
                    let models = root.join("models");
                    // Retain legacy cache locations so existing resumable bytes remain usable.
                    // The mapping belongs to the role, never to catalog/display ordering.
                    let cache = if row.role == Role::Speaker {
                        models.join("ocr/.downloads")
                    } else {
                        models.join(".downloads").join(match row.role {
                            Role::Asr => "0",
                            Role::Translation => "1",
                            Role::LanguageIdentification => "2",
                            Role::Speaker => unreachable!(),
                        })
                    };
                    let mut initial = true;
                    let mut voice_check = 0;
                    while !worker_quit.load(Ordering::Relaxed) {
                        let verify = if row.role.voice_only() {
                            let requested = shared.check_voice.load(Ordering::Acquire);
                            let verify = requested != voice_check
                                && shared.voice_enabled.load(Ordering::Acquire);
                            voice_check = requested;
                            verify
                        } else {
                            std::mem::take(&mut initial)
                        };
                        if verify {
                            shared.update(index, |s| s.state = State::Checking);
                            let result = (|| -> Result<()> {
                                fs::create_dir_all(&cache)?;
                                let _lock = transaction_lock(&cache)?;
                                cleanup_staging(&cache)?;
                                repair_manifests(&models, &cache, &row)?;
                                let ready = installed(&models, &row, &worker_quit)?;
                                let received = cached_bytes(&cache, &models, &row);
                                shared.update(index, |s| {
                                    s.received = if ready { s.total } else { received };
                                    s.state = if ready { State::Ready } else { State::Idle };
                                });
                                Ok(())
                            })();
                            if let Err(error) = result {
                                shared.update(index, |s| {
                                    s.state = State::Error(format!("{error:#}"))
                                });
                            }
                        }
                        match rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(Command::Start) => {
                                if row.role.voice_only()
                                    && !shared.voice_enabled.load(Ordering::Acquire)
                                {
                                    shared.update(index, |s| s.state = State::Idle);
                                    continue;
                                }
                                let result = rt.block_on(install(
                                    &client,
                                    &models,
                                    &cache,
                                    &row,
                                    &worker_cancel,
                                    &worker_quit,
                                    Progress {
                                        shared: &shared,
                                        index,
                                    },
                                ));
                                shared.update(index, |s| {
                                    s.state = match result {
                                        Ok(()) => State::Ready,
                                        Err(_) if stopped(&worker_cancel, &worker_quit) => {
                                            State::Idle
                                        }
                                        Err(e) => State::Error(format!("{e:#}")),
                                    }
                                });
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => (),
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    shared.update(index, |s| s.state = State::Error(format!("{e:#}")));
                }
            });
            shutdown.lock().unwrap().push((quit, worker));
            controls.push((tx, cancel));
        }
        Self { shared, controls }
    }
    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.shared.snapshots.lock().unwrap().clone()
    }
    pub fn take_changed(&self) -> bool {
        self.shared.changed.swap(false, Ordering::AcqRel)
    }
    pub fn all_ready(&self, voice_enabled: bool) -> bool {
        self.shared
            .snapshots
            .lock()
            .unwrap()
            .iter()
            .all(|s| (!voice_enabled && s.voice_only) || s.state == State::Ready)
    }
    pub fn set_voice_enabled(&self, enabled: bool) {
        if self.shared.voice_enabled.swap(enabled, Ordering::AcqRel) == enabled {
            return;
        }
        let mut snapshots = self.shared.snapshots.lock().unwrap();
        for (index, snapshot) in snapshots.iter_mut().enumerate() {
            if !snapshot.voice_only {
                continue;
            }
            let (_, cancel) = &self.controls[index];
            if enabled && snapshot.state != State::Ready {
                self.shared.check_voice.fetch_add(1, Ordering::Release);
                if !snapshot.state.active() {
                    snapshot.state = State::Checking;
                }
                // The existing worker observes this request on its next idle iteration.
            } else if matches!(
                snapshot.state,
                State::Downloading | State::Verifying | State::Installing
            ) {
                cancel.store(true, Ordering::Release);
                snapshot.state = State::Stopping;
            }
        }
        self.shared.changed.store(true, Ordering::Release);
    }
    pub fn toggle(&self, index: usize) {
        let Some((tx, cancel)) = self.controls.get(index) else {
            return;
        };
        let mut snapshots = self.shared.snapshots.lock().unwrap();
        let s = &mut snapshots[index];
        if s.voice_only && !self.shared.voice_enabled.load(Ordering::Acquire) {
            return;
        }
        match s.state {
            State::Downloading | State::Verifying | State::Installing => {
                cancel.store(true, Ordering::Release);
                s.state = State::Stopping;
            }
            State::Idle | State::Error(_) => {
                cancel.store(false, Ordering::Release);
                s.state = State::Downloading;
                if tx.try_send(Command::Start).is_err() {
                    s.state = State::Error(
                        "Download worker stopped. Fix the folder permissions and reopen the app."
                            .into(),
                    );
                }
            }
            _ => return,
        }
        self.shared.changed.store(true, Ordering::Release);
    }
}
fn total(row: &Row) -> u64 {
    if row.role == Role::Speaker {
        return speaker::total();
    }
    row.archive
        .as_ref()
        .map_or_else(|| row.files.iter().map(|f| f.bytes).sum(), |a| a.bytes)
}
fn stopped(cancel: &AtomicBool, quit: &AtomicBool) -> bool {
    cancel.load(Ordering::Acquire) || quit.load(Ordering::Acquire)
}
fn check(cancel: &AtomicBool, quit: &AtomicBool) -> Result<()> {
    ensure!(!stopped(cancel, quit), "Download stopped");
    Ok(())
}
fn digest(
    path: &Path,
    expected: &Artifact,
    cancel: &AtomicBool,
    quit: &AtomicBool,
) -> Result<bool> {
    check(cancel, quit)?;
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if f.metadata()?.len() != expected.bytes {
        return Ok(false);
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        check(cancel, quit)?;
        let n = f.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()) == expected.sha256)
}
fn installed(models: &Path, row: &Row, quit: &AtomicBool) -> Result<bool> {
    if row.role == Role::Speaker {
        let root = models.join("ocr");
        for f in &row.files {
            if !digest(&root.join(&f.path), f, quit, quit)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    for f in &row.files {
        if !digest(&models.join(&f.path), f, quit, quit)? {
            return Ok(false);
        }
    }
    Ok(true)
}
fn cached_bytes(cache: &Path, models: &Path, row: &Row) -> u64 {
    if row.role == Role::Speaker {
        return speaker::cached_bytes(&models.join("ocr"), cache);
    }
    if let Some(a) = &row.archive {
        return fs::metadata(cache.join("archive.part")).map_or(0, |m| m.len().min(a.bytes));
    }
    row.files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let dest = models.join(&f.path);
            if fs::metadata(dest).is_ok_and(|m| m.len() == f.bytes) {
                f.bytes
            } else {
                fs::metadata(cache.join(format!("{i}.part"))).map_or(0, |m| m.len().min(f.bytes))
            }
        })
        .sum()
}
fn transaction_lock(cache: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cache.join(".lock"))?;
    match file.try_lock() {
        Ok(()) => (),
        Err(std::fs::TryLockError::WouldBlock) => bail!(
            "Another CS Translator instance is checking/installing this model; retry when it finishes"
        ),
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    Ok(file)
}
fn cleanup_staging(cache: &Path) -> Result<()> {
    for entry in fs::read_dir(cache)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "install") {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}
struct Staging<'a>(&'a Path);
impl Drop for Staging<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
fn repair_manifests(models: &Path, cache: &Path, row: &Row) -> Result<()> {
    if row.role == Role::Speaker {
        // Missing assets are normal during onboarding; config creation is safe
        // and never replaces an existing custom configuration.
        return speaker::write_config(&models.join("ocr"), cache);
    }
    if row.role == Role::Translation {
        return translation_manifest(models, cache, &row.id);
    }
    if row.role != Role::Asr {
        return Ok(());
    }
    let root = models.parent().context("missing installation root")?;
    let path = root
        .join("model-manifests")
        .join(format!("{}.toml", row.id));
    let temp = cache.join("asr.toml.install");
    let _cleanup = Staging(&temp);
    let mut f = File::create(&temp)?;
    f.write_all(include_str!("../../../model-manifests/nemotron-multilingual.toml").as_bytes())?;
    f.sync_all()?;
    drop(f);
    commit(&temp, &path)
}
fn commit(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest.parent().context("missing installation parent")?)?;
    // std::fs::rename replaces existing files atomically on both Windows and Unix.
    fs::rename(source, dest).with_context(|| format!("Installing {}", dest.display()))
}
async fn cancellation(cancel: &AtomicBool, quit: &AtomicBool) {
    while !stopped(cancel, quit) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
fn range_start(value: &str, start: u64, total: u64) -> bool {
    let Some(value) = value.strip_prefix("bytes ") else {
        return false;
    };
    let Some((range, size)) = value.split_once('/') else {
        return false;
    };
    let Some((first, last)) = range.split_once('-') else {
        return false;
    };
    first.parse::<u64>() == Ok(start)
        && last.parse::<u64>() == Ok(total - 1)
        && size.parse::<u64>() == Ok(total)
}
async fn transfer(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    a: &Artifact,
    cancel: &AtomicBool,
    quit: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    check(cancel, quit)?;
    let mut offset = fs::metadata(path).map_or(0, |m| m.len());
    if offset > a.bytes {
        File::create(path)?;
        offset = 0;
    }
    if offset == a.bytes {
        progress(offset);
        return Ok(());
    }
    let mut request = client
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity");
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let mut response = tokio::select! { biased; _=cancellation(cancel,quit)=>bail!("Download stopped"), r=request.send()=>r.context("Connecting to model host")? };
    let status = response.status();
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        ensure!(
            range_start(
                response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
                offset,
                a.bytes
            ),
            "Invalid Content-Range; partial retained"
        );
    } else if status == reqwest::StatusCode::OK {
        offset = 0;
    } else {
        bail!("Model host returned HTTP {status}")
    }
    if let Some(length) = response.content_length() {
        ensure!(length == a.bytes - offset, "Unexpected model response size");
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .append(offset > 0)
        .open(path)?;
    progress(offset);
    loop {
        let chunk = tokio::select! { biased; _=cancellation(cancel,quit)=>bail!("Download stopped"), r=response.chunk()=>r.context("Reading model response")? };
        let Some(chunk) = chunk else { break };
        check(cancel, quit)?;
        ensure!(
            chunk.len() as u64 <= a.bytes - offset,
            "Model response exceeds pinned size"
        );
        // reqwest frames may be larger; the disk/hash buffers remain bounded.
        file.write_all(&chunk)?;
        offset += chunk.len() as u64;
        progress(offset);
    }
    file.sync_all()?;
    ensure!(
        offset == a.bytes,
        "Incomplete model response ({offset}/{})",
        a.bytes
    );
    Ok(())
}
async fn verified_transfer(
    client: &reqwest::Client,
    path: &Path,
    artifact: (&Artifact, &str),
    cancel: &AtomicBool,
    quit: &AtomicBool,
    progress: Progress<'_>,
    base: u64,
) -> Result<()> {
    let Progress { shared, index } = progress;
    let (a, url) = artifact;
    shared.update(index, |s| s.state = State::Downloading);
    let mut published = std::time::Instant::now();
    transfer(client, url, path, a, cancel, quit, |n| {
        if n == a.bytes || published.elapsed() >= Duration::from_millis(100) {
            shared.update(index, |s| s.received = base + n);
            published = std::time::Instant::now();
        }
    })
    .await?;
    shared.update(index, |s| s.state = State::Verifying);
    if !digest(path, a, cancel, quit)? {
        fs::remove_file(path)?;
        bail!("SHA256 mismatch; corrupt partial removed. Start to retry.")
    }
    Ok(())
}
async fn install(
    client: &reqwest::Client,
    models: &Path,
    cache: &Path,
    row: &Row,
    cancel: &AtomicBool,
    quit: &AtomicBool,
    progress: Progress<'_>,
) -> Result<()> {
    if row.role == Role::Speaker {
        return speaker::acquire(
            client,
            &models.join("ocr"),
            cache,
            &speaker::catalog(),
            cancel,
            quit,
            progress,
        )
        .await;
    }
    let Progress { shared, index } = progress;
    let _lock = transaction_lock(cache)?;
    cleanup_staging(cache)?;
    shared.update(index, |s| s.state = State::Verifying);
    // Recheck under the transaction lock: another instance or a completed Stop
    // race may already have installed every byte. Never request the archive again.
    if installed(models, row, quit)? {
        repair_manifests(models, cache, row)?;
        shared.update(index, |s| {
            s.received = s.total;
            s.state = State::Ready;
        });
        return Ok(());
    }
    if let Some(archive) = &row.archive {
        let path = cache.join("archive.part");
        let artifact = Artifact {
            path: String::new(),
            bytes: archive.bytes,
            sha256: archive.sha256.clone(),
        };
        verified_transfer(
            client,
            &path,
            (&artifact, &archive.url),
            cancel,
            quit,
            progress,
            0,
        )
        .await?;
        shared.update(index, |s| s.state = State::Installing);
        extract(&path, models, cache, &row.files, cancel, quit)?;
        fs::remove_file(path)?;
    } else {
        let mut base = 0;
        for (i, a) in row.files.iter().enumerate() {
            check(cancel, quit)?;
            let dest = models.join(&a.path);
            shared.update(index, |s| s.state = State::Verifying);
            if !digest(&dest, a, cancel, quit)? {
                let path = cache.join(format!("{i}.part"));
                verified_transfer(
                    client,
                    &path,
                    (a, &row.urls[i]),
                    cancel,
                    quit,
                    progress,
                    base,
                )
                .await?;
                check(cancel, quit)?;
                shared.update(index, |s| s.state = State::Installing);
                commit(&path, &dest)?;
            }
            base += a.bytes;
            shared.update(index, |s| s.received = base);
        }
    }
    // All required commits won; a late Stop cannot demote a complete install.
    repair_manifests(models, cache, row)?;
    shared.update(index, |s| {
        s.received = s.total;
        s.state = State::Ready;
    });
    Ok(())
}
fn translation_manifest(models: &Path, cache: &Path, directory: &str) -> Result<()> {
    let temp = cache.join("translation.toml.install");
    let _cleanup = Staging(&temp);
    let mut f = File::create(&temp)?;
    f.write_all(include_str!("../../../model-manifests/translation.toml").as_bytes())?;
    f.sync_all()?;
    drop(f);
    commit(&temp, &models.join(directory).join("translation.toml"))
}
fn extract(
    path: &Path,
    models: &Path,
    cache: &Path,
    files: &[Artifact],
    cancel: &AtomicBool,
    quit: &AtomicBool,
) -> Result<()> {
    let decoder = bzip2::read::BzDecoder::new(File::open(path)?);
    let mut archive = tar::Archive::new(decoder);
    let mut found = vec![false; files.len()];
    for entry in archive.entries()? {
        check(cancel, quit)?;
        let mut entry = entry?;
        let member = entry.path()?.to_string_lossy().replace('\\', "/");
        let Some((i, a)) = files.iter().enumerate().find(|(_, a)| a.path == member) else {
            // Explicit draining makes skipped archive members interruptible too.
            let mut buffer = [0u8; 65536];
            loop {
                check(cancel, quit)?;
                if entry.read(&mut buffer)? == 0 {
                    break;
                }
            }
            continue;
        };
        ensure!(
            !found[i] && entry.header().entry_type().is_file(),
            "Invalid duplicate/non-file archive member"
        );
        ensure!(entry.size() == a.bytes, "Archive member size mismatch");
        let temp = cache.join(format!("member-{i}.install"));
        let _cleanup = Staging(&temp);
        let mut out = File::create(&temp)?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut bytes = 0;
        loop {
            check(cancel, quit)?;
            let n = entry.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            hash.update(&buffer[..n]);
            out.write_all(&buffer[..n])?;
        }
        ensure!(
            bytes == a.bytes && format!("{:x}", hash.finalize()) == a.sha256,
            "Archive member SHA256 mismatch"
        );
        out.sync_all()?;
        drop(out);
        check(cancel, quit)?;
        commit(&temp, &models.join(&a.path))?;
        found[i] = true;
        if found.iter().all(|v| *v) {
            break;
        }
    }
    ensure!(
        found.iter().all(|v| *v),
        "Archive is missing required model files"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "cs-download-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn artifact() -> Artifact {
        Artifact {
            path: "model".into(),
            bytes: 6,
            sha256: format!("{:x}", Sha256::digest(b"abcdef")),
        }
    }
    fn server(response: &'static [u8]) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream.write_all(response).unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{addr}/model"), worker)
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    #[test]
    fn text_only_startup_defers_speech_inspection_until_enabled() {
        let temp = Temp::new();
        let shutdown: crate::session::ShutdownWorkers = Arc::new(Mutex::new(Vec::new()));
        let manager = Manager::new(temp.0.clone(), false, &shutdown);
        let wait_idle = || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while manager.snapshots().iter().any(|s| s.state.active()) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "model check did not settle"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        wait_idle();
        let before = manager.snapshots();
        assert!(!temp.0.join("models/.downloads/0").exists());
        assert!(!temp.0.join("models/ocr/.downloads").exists());
        assert_eq!(before.iter().filter(|s| s.voice_only).count(), 2);
        manager.set_voice_enabled(true);
        wait_idle();
        assert!(temp.0.join("models/.downloads/0").is_dir());
        assert!(temp.0.join("models/ocr/.downloads").is_dir());
        let after = manager.snapshots();
        for (before, after) in before.iter().zip(&after) {
            assert_eq!(after.state, State::Idle);
            assert_eq!(after.received, 0);
            if !before.voice_only {
                assert_eq!(before.state, after.state);
            }
        }
        // Enabling only inspects local assets; it never starts acquisition.
        assert!(!manager.all_ready(true));
        for (quit, worker) in shutdown.lock().unwrap().drain(..) {
            quit.store(true, Ordering::Release);
            worker.join().unwrap();
        }
    }

    #[test]
    fn mode_changes_gate_and_cancel_both_voice_rows_without_affecting_text_models() {
        let controls: Vec<_> = (0..3)
            .map(|_| {
                let (tx, _rx) = crossbeam_channel::bounded(1);
                (tx, Arc::new(AtomicBool::new(false)))
            })
            .collect();
        let manager = Manager {
            shared: Arc::new(Shared {
                snapshots: Mutex::new(vec![
                    Snapshot {
                        name: String::new(),
                        detail: String::new(),
                        received: 5,
                        total: 10,
                        state: State::Downloading,
                        voice_only: true,
                    },
                    Snapshot {
                        name: String::new(),
                        detail: String::new(),
                        received: 10,
                        total: 10,
                        state: State::Ready,
                        voice_only: false,
                    },
                    Snapshot {
                        name: "Speaker detection".into(),
                        detail: String::new(),
                        received: 3,
                        total: 10,
                        state: State::Installing,
                        voice_only: true,
                    },
                ]),
                changed: AtomicBool::new(false),
                voice_enabled: AtomicBool::new(true),
                check_voice: AtomicUsize::new(0),
            }),
            controls,
        };
        manager.set_voice_enabled(false);
        assert!(manager.all_ready(false));
        assert!(!manager.all_ready(true));
        assert!(manager.controls[0].1.load(Ordering::Acquire));
        assert!(!manager.controls[1].1.load(Ordering::Acquire));
        assert!(manager.controls[2].1.load(Ordering::Acquire));
        manager.set_voice_enabled(true);
        assert_ne!(manager.shared.check_voice.load(Ordering::Acquire), 0);
        // Re-enabling must not revive an in-flight cancelled transaction.
        assert!(manager.controls[0].1.load(Ordering::Acquire));
        assert_eq!(manager.snapshots()[0].state, State::Stopping);
        assert_eq!(manager.snapshots()[1].state, State::Ready);
        assert_eq!(manager.snapshots()[2].state, State::Stopping);
    }

    #[test]
    fn resumes_only_valid_ranges_and_resets_on_200() {
        for response in [
            &b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef"[..],
        ] {
            let temp=Temp::new();let path=temp.0.join("partial");fs::write(&path,b"abc").unwrap();
            let (url,server)=server(response);let cancel=AtomicBool::new(false);
            runtime().block_on(transfer(&reqwest::Client::new(),&url,&path,&artifact(),&cancel,&cancel,|_|{})).unwrap();
            assert_eq!(fs::read(&path).unwrap(),b"abcdef");
            assert!(server.join().unwrap().to_ascii_lowercase().contains("range: bytes=3-"));
        }
    }
    #[test]
    fn rejects_wrong_range_and_short_body_without_destroying_partial() {
        let temp = Temp::new();
        let path = temp.0.join("partial");
        fs::write(&path, b"abc").unwrap();
        let (url,handle)=server(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\nxyz");
        let cancel = AtomicBool::new(false);
        assert!(
            runtime()
                .block_on(transfer(
                    &reqwest::Client::new(),
                    &url,
                    &path,
                    &artifact(),
                    &cancel,
                    &cancel,
                    |_| {}
                ))
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"abc");
        handle.join().unwrap();
        let (url,server)=server(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\nd");
        assert!(
            runtime()
                .block_on(transfer(
                    &reqwest::Client::new(),
                    &url,
                    &path,
                    &artifact(),
                    &cancel,
                    &cancel,
                    |_| {}
                ))
                .is_err()
        );
        assert!(fs::read(&path).unwrap().starts_with(b"abc"));
        server.join().unwrap();
    }
    #[test]
    fn corrupt_complete_partial_is_removed_before_install() {
        let temp = Temp::new();
        let path = temp.0.join("partial");
        fs::write(&path, b"xxxxxx").unwrap();
        let cancel = AtomicBool::new(false);
        let shared = Shared {
            snapshots: Mutex::new(vec![Snapshot {
                name: String::new(),
                detail: String::new(),
                received: 0,
                total: 6,
                state: State::Idle,
                voice_only: false,
            }]),
            changed: AtomicBool::new(false),
            voice_enabled: AtomicBool::new(true),
            check_voice: AtomicUsize::new(0),
        };
        assert!(
            runtime()
                .block_on(verified_transfer(
                    &reqwest::Client::new(),
                    &path,
                    (&artifact(), ""),
                    &cancel,
                    &cancel,
                    Progress {
                        shared: &shared,
                        index: 0
                    },
                    0
                ))
                .is_err()
        );
        assert!(!path.exists());
    }
    #[test]
    fn cancellation_interrupts_stalled_http_and_preserves_partial() {
        let temp = Temp::new();
        let path = temp.0.join("partial");
        fs::write(&path, b"abc").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut byte = [0];
            let mut req = Vec::new();
            while !req.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                req.push(byte[0]);
            }
            stop.store(true, Ordering::Release);
            let _ = done_rx.recv_timeout(Duration::from_secs(2));
        });
        let now = std::time::Instant::now();
        assert!(
            runtime()
                .block_on(transfer(
                    &reqwest::Client::new(),
                    &format!("http://{addr}/model"),
                    &path,
                    &artifact(),
                    &cancel,
                    &AtomicBool::new(false),
                    |_| {}
                ))
                .is_err()
        );
        assert!(now.elapsed() < Duration::from_secs(1));
        assert_eq!(fs::read(&path).unwrap(), b"abc");
        done_tx.send(()).unwrap();
        server.join().unwrap();
    }
    #[test]
    fn installed_archive_wins_late_stop_without_network_or_readme() {
        let temp = Temp::new();
        let models = temp.0.join("models");
        let cache = temp.0.join("cache");
        fs::create_dir_all(&models).unwrap();
        fs::create_dir_all(&cache).unwrap();
        fs::write(models.join("model"), b"abcdef").unwrap();
        let row = Row {
            id: "fixture".into(),
            role: Role::LanguageIdentification,
            name: String::new(),
            detail: String::new(),
            archive: Some(Archive {
                url: "http://127.0.0.1:0/no-request".into(),
                bytes: 6,
                sha256: artifact().sha256,
            }),
            files: vec![artifact()],
            urls: Vec::new(),
        };
        let shared = Shared {
            snapshots: Mutex::new(vec![Snapshot {
                name: String::new(),
                detail: String::new(),
                received: 0,
                total: 6,
                state: State::Stopping,
                voice_only: true,
            }]),
            changed: AtomicBool::new(false),
            voice_enabled: AtomicBool::new(true),
            check_voice: AtomicUsize::new(0),
        };
        runtime()
            .block_on(install(
                &reqwest::Client::new(),
                &models,
                &cache,
                &row,
                &AtomicBool::new(true),
                &AtomicBool::new(false),
                Progress {
                    shared: &shared,
                    index: 0,
                },
            ))
            .unwrap();
        assert_eq!(shared.snapshots.lock().unwrap()[0].state, State::Ready);
        assert!(!models.join("README.md").exists());
        assert_eq!(fs::read(models.join("model")).unwrap(), b"abcdef");
    }

    #[test]
    fn abandoned_staging_cleanup_preserves_resumable_bytes() {
        let temp = Temp::new();
        fs::write(temp.0.join("archive.part"), b"abc").unwrap();
        fs::write(temp.0.join("0.part"), b"ab").unwrap();
        fs::write(temp.0.join("member-0.install"), b"not resumable").unwrap();
        let lock = transaction_lock(&temp.0).unwrap();
        cleanup_staging(&temp.0).unwrap();
        assert!(!temp.0.join("member-0.install").exists());
        assert_eq!(fs::read(temp.0.join("archive.part")).unwrap(), b"abc");
        assert_eq!(fs::read(temp.0.join("0.part")).unwrap(), b"ab");
        assert!(transaction_lock(&temp.0).is_err());
        drop(lock);
        assert!(transaction_lock(&temp.0).is_ok());
        assert!(temp.0.join(".lock").exists());
    }

    #[test]
    fn cache_lock_child() {
        let Some(path) = std::env::var_os("CS_TRANSLATOR_LOCK_TEST") else {
            return;
        };
        assert!(transaction_lock(Path::new(&path)).is_err());
        assert_eq!(
            fs::read(Path::new(&path).join("archive.part")).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn cache_lock_excludes_other_processes_without_touching_partial() {
        let temp = Temp::new();
        fs::write(temp.0.join("archive.part"), b"abc").unwrap();
        let lock = transaction_lock(&temp.0).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "downloads::tests::cache_lock_child",
                "--nocapture",
            ])
            .env("CS_TRANSLATOR_LOCK_TEST", &temp.0)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read(temp.0.join("archive.part")).unwrap(), b"abc");
        drop(lock);
        assert!(transaction_lock(&temp.0).is_ok());
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    #[test]
    fn installs_only_selected_members_and_never_commits_corruption() {
        let root = std::env::temp_dir().join(format!("cs-archive-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("archive");
        let models = root.join("models");
        let cache = root.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let encoder =
            bzip2::write::BzEncoder::new(File::create(&path).unwrap(), bzip2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, data) in [
            ("ignored/large.bin", &b"not selected"[..]),
            ("selected/model", &b"abcdef"[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        let cancel = AtomicBool::new(false);
        let mut selected = Artifact {
            path: "selected/model".into(),
            bytes: 6,
            sha256: "incorrect".into(),
        };
        assert!(
            extract(
                &path,
                &models,
                &cache,
                &[selected.clone()],
                &cancel,
                &cancel
            )
            .is_err()
        );
        assert!(!models.join("selected/model").exists());
        assert!(!cache.join("member-0.install").exists());
        assert!(
            path.exists(),
            "reusable archive must survive extraction failure"
        );
        selected.sha256 = format!("{:x}", Sha256::digest(b"abcdef"));
        fs::create_dir_all(models.join("selected/model")).unwrap();
        assert!(
            extract(
                &path,
                &models,
                &cache,
                &[selected.clone()],
                &cancel,
                &cancel
            )
            .is_err()
        );
        assert!(
            !cache.join("member-0.install").exists(),
            "failed commit must clean staging"
        );
        assert!(path.exists());
        fs::remove_dir(models.join("selected/model")).unwrap();
        extract(
            &path,
            &models,
            &cache,
            &[selected.clone()],
            &cancel,
            &cancel,
        )
        .unwrap();
        assert_eq!(fs::read(models.join("selected/model")).unwrap(), b"abcdef");
        assert!(!models.join("ignored").exists());
        cancel.store(true, Ordering::Release);
        assert!(digest(&models.join("selected/model"), &selected, &cancel, &cancel).is_err());
        assert!(extract(&path, &models, &cache, &[selected], &cancel, &cancel).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
