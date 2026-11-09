use super::*;

const CONFIG: &str = include_str!("../../../platform/ocr.example.toml");

// Only this exact formerly generated template is eligible for migration.
// Any user edits, including custom icon/geometry settings, remain untouched.
const LEGACY_CONFIG: &str = r#"# Copy beside explicitly downloaded model files; paths are relative to this file.
# Source: https://github.com/GreatV/oar-ocr/releases/tag/v0.3.0
# PaddleOCR weights/dictionaries and OAR: Apache-2.0.
# Native CPU runtime (MIT): https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-win-x64-1.23.2.zip
# Copy lib/onnxruntime.dll as onnxruntime-ocr.dll to avoid the ASR runtime DLL.
# speaker-icon.png is a tight real screenshot crop of the ACTIVE speaker glyph,
# with a narrow background border, not an avatar or name. Recalibrate for HUD scale.
# No matches means no players; there is deliberately no generic-text fallback.
runtime = "onnxruntime-ocr.dll"
speaker_icon = "speaker-icon.png"
icon_threshold = 0.82
# Scale relative to the calibrated glyph; only ±5% neighbors are searched.
# Broad arbitrary-size searches can confuse ordinary chat glyphs for speakers.
icon_scale = 1.0
# Distance from speaker icon left edge to name, in icon heights (skips avatar).
name_offset_icon_heights = 3.6
# Optional hard field width, measured in icon heights; zero retains visible text.
# Set this on a calibrated HUD where the name and location are separate rectangles.
name_width_icon_heights = 0.0
# CS2's spaced " @ " separates name/location. Disable if this HUD uses it inside names:
# without independent layout evidence those two interpretations are ambiguous.
strip_location_field = true
# Speaker glyph left-edge search band in pixels relative to the cropped ROI.
# Calibrate these bounds when moving/rescaling the HUD; all y positions are searched.
icon_band_left = 0
icon_band_right = 64
# Icon-aligned rows go directly to native recognition; no detector model is used.
# Cached row pixels skip recognition when only unrelated HUD/world pixels change.
threads = 2
minimum_confidence = 0.55
capture_interval_ms = 125
upscale = 2

[[recognizers]]
model = "pp-ocrv5_mobile_rec.onnx"
dictionary = "ppocrv5_dict.txt"

[[recognizers]]
model = "cyrillic_pp-ocrv5_mobile_rec.onnx"
dictionary = "ppocrv5_cyrillic_dict.txt"
"#;

#[derive(Deserialize)]
struct RuntimeMember {
    #[serde(flatten)]
    artifact: Artifact,
    member: String,
}
#[derive(Deserialize)]
struct Runtime {
    archive: DownloadFile,
    files: Vec<RuntimeMember>,
}
#[derive(Deserialize)]
pub(super) struct SpeakerCatalog {
    schema_version: u32,
    files: Vec<DownloadFile>,
    runtime: Runtime,
}

pub(super) fn catalog() -> SpeakerCatalog {
    let catalog: SpeakerCatalog = serde_json::from_str(include_str!(
        "../../../../model-manifests/ocr-downloads.json"
    ))
    .expect("embedded OCR catalog");
    assert_eq!(catalog.schema_version, 1);
    catalog
}

pub(super) fn row() -> Row {
    let catalog = catalog();
    Row {
        id: "speaker-detection".into(),
        role: Role::Speaker,
        name: "Speaker detection".into(),
        detail: "HUD name recognition and private OCR runtime".into(),
        archive: None,
        files: catalog
            .files
            .iter()
            .map(|f| f.artifact.clone())
            .chain(catalog.runtime.files.iter().map(|f| f.artifact.clone()))
            .collect(),
        urls: Vec::new(),
    }
}

pub(super) fn total() -> u64 {
    let catalog = catalog();
    catalog.runtime.archive.artifact.bytes
        + catalog.files.iter().map(|f| f.artifact.bytes).sum::<u64>()
}

pub(super) fn cached_bytes(root: &Path, cache: &Path) -> u64 {
    let catalog = catalog();
    let runtime = &catalog.runtime;
    let runtime_ready = runtime.files.iter().all(|f| {
        fs::metadata(root.join(&f.artifact.path)).is_ok_and(|m| m.len() == f.artifact.bytes)
    });
    let runtime_bytes = if runtime_ready {
        runtime.archive.artifact.bytes
    } else {
        fs::metadata(cache.join("runtime.zip.part"))
            .map_or(0, |m| m.len().min(runtime.archive.artifact.bytes))
    };
    runtime_bytes
        + catalog
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if fs::metadata(root.join(&f.artifact.path))
                    .is_ok_and(|m| m.len() == f.artifact.bytes)
                {
                    f.artifact.bytes
                } else {
                    fs::metadata(cache.join(format!("ocr-{i}.part")))
                        .map_or(0, |m| m.len().min(f.artifact.bytes))
                }
            })
            .sum::<u64>()
}

fn verify(
    root: &Path,
    catalog: &SpeakerCatalog,
    cancel: &AtomicBool,
    quit: &AtomicBool,
) -> Result<bool> {
    for artifact in catalog
        .files
        .iter()
        .map(|f| &f.artifact)
        .chain(catalog.runtime.files.iter().map(|f| &f.artifact))
    {
        if !digest(&root.join(&artifact.path), artifact, cancel, quit)? {
            return Ok(false);
        }
    }
    Ok(true)
}
pub(super) fn write_config(root: &Path, cache: &Path) -> Result<()> {
    let dest = root.join("ocr.toml");
    match fs::read(&dest) {
        Ok(bytes)
            if bytes == LEGACY_CONFIG.as_bytes()
                || LEGACY_CONFIG
                    .bytes()
                    .flat_map(|byte| {
                        if byte == b'\n' {
                            [Some(b'\r'), Some(b'\n')]
                        } else {
                            [Some(byte), None]
                        }
                    })
                    .flatten()
                    .eq(bytes.iter().copied()) => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let temp = cache.join("ocr.toml.install");
    let _cleanup = Staging(&temp);
    let mut out = File::create(&temp)?;
    out.write_all(CONFIG.as_bytes())?;
    out.sync_all()?;
    drop(out);
    commit(&temp, &dest)
}
pub(super) async fn acquire(
    client: &reqwest::Client,
    root: &Path,
    cache: &Path,
    catalog: &SpeakerCatalog,
    cancel: &AtomicBool,
    quit: &AtomicBool,
    progress: Progress<'_>,
) -> Result<()> {
    fs::create_dir_all(cache)?;
    let _lock = transaction_lock(cache)?;
    cleanup_staging(cache)?;
    let Progress {
        shared,
        index: row_index,
    } = progress;
    let runtime_ready =
        catalog
            .runtime
            .files
            .iter()
            .try_fold(true, |ready, f| -> Result<bool> {
                Ok(digest(&root.join(&f.artifact.path), &f.artifact, cancel, quit)? && ready)
            })?;
    let archive = &catalog.runtime.archive;
    if !runtime_ready {
        let path = cache.join("runtime.zip.part");
        verified_transfer(
            client,
            &path,
            (&archive.artifact, &archive.url),
            cancel,
            quit,
            progress,
            0,
        )
        .await?;
        shared.update(row_index, |s| s.state = State::Installing);
        extract_runtime(&path, root, cache, &catalog.runtime.files, cancel, quit)?;
    }
    let mut base = archive.artifact.bytes;
    for (index, file) in catalog.files.iter().enumerate() {
        check(cancel, quit)?;
        let a = &file.artifact;
        if !digest(&root.join(&a.path), a, cancel, quit)? {
            let partial = cache.join(format!("ocr-{index}.part"));
            verified_transfer(
                client,
                &partial,
                (a, &file.url),
                cancel,
                quit,
                progress,
                base,
            )
            .await?;
            shared.update(row_index, |s| s.state = State::Installing);
            check(cancel, quit)?;
            commit(&partial, &root.join(&a.path))?;
        }
        base += a.bytes;
        shared.update(row_index, |s| s.received = base);
    }
    shared.update(row_index, |s| s.state = State::Verifying);
    ensure!(
        verify(root, catalog, cancel, quit)?,
        "Installed OCR assets failed verification"
    );
    check(cancel, quit)?;
    write_config(root, cache)
}

fn extract_runtime(
    path: &Path,
    root: &Path,
    cache: &Path,
    files: &[RuntimeMember],
    cancel: &AtomicBool,
    quit: &AtomicBool,
) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(path)?)?;
    let mut found = vec![false; files.len()];
    for index in 0..archive.len() {
        check(cancel, quit)?;
        let mut entry = archive.by_index(index)?;
        let Some((i, pinned)) = files
            .iter()
            .enumerate()
            .find(|(_, f)| f.member == entry.name())
        else {
            continue;
        };
        ensure!(
            !found[i]
                && entry.is_file()
                && entry.enclosed_name().is_some()
                && entry
                    .unix_mode()
                    .is_none_or(|mode| mode & 0o170000 != 0o120000),
            "Invalid runtime archive member"
        );
        let a = &pinned.artifact;
        ensure!(entry.size() == a.bytes, "Runtime member size mismatch");
        let temp = cache.join(format!("runtime-{i}.install"));
        let _cleanup = Staging(&temp);
        let mut out = File::create(&temp)?;
        let mut hash = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            check(cancel, quit)?;
            let n = entry.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            ensure!(bytes <= a.bytes, "Runtime member exceeds pinned size");
            hash.update(&buffer[..n]);
            out.write_all(&buffer[..n])?;
        }
        ensure!(
            bytes == a.bytes && format!("{:x}", hash.finalize()) == a.sha256,
            "Runtime member SHA256 mismatch"
        );
        out.sync_all()?;
        drop(out);
        check(cancel, quit)?;
        commit(&temp, &root.join(&a.path))?;
        found[i] = true;
    }
    ensure!(
        found.iter().all(|v| *v),
        "Runtime archive missing pinned files"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_repair_preserves_user_ocr_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".downloads");
        fs::create_dir_all(&cache).unwrap();
        let path = dir.path().join("ocr.toml");
        fs::write(&path, "threads = 1\ncapture_interval_ms = 333\n").unwrap();
        write_config(dir.path(), &cache).unwrap();
        let restored: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(restored["threads"].as_integer(), Some(1));
        assert_eq!(restored["capture_interval_ms"].as_integer(), Some(333));
    }

    #[test]
    fn migrates_only_the_exact_legacy_generated_template() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".downloads");
        fs::create_dir_all(&cache).unwrap();
        let path = dir.path().join("ocr.toml");
        for legacy in [
            LEGACY_CONFIG.to_owned(),
            LEGACY_CONFIG.replace('\n', "\r\n"),
        ] {
            fs::write(&path, legacy).unwrap();
            write_config(dir.path(), &cache).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), CONFIG);
        }
        let customized = LEGACY_CONFIG.replace("icon_scale = 1.0", "icon_scale = 1.25");
        fs::write(&path, &customized).unwrap();
        write_config(dir.path(), &cache).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), customized);
    }

    #[test]
    fn runtime_extracts_only_pinned_members_and_rejects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".downloads");
        fs::create_dir_all(&cache).unwrap();
        let archive = cache.join("runtime.zip.part");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("../untrusted.dll", options).unwrap();
        writer.write_all(b"not installed").unwrap();
        writer
            .start_file("runtime/lib/runtime.dll", options)
            .unwrap();
        writer.write_all(b"runtime").unwrap();
        writer.finish().unwrap();
        let mut files = vec![RuntimeMember {
            member: "runtime/lib/runtime.dll".into(),
            artifact: Artifact {
                path: "runtime.dll".into(),
                bytes: 7,
                sha256: format!("{:x}", Sha256::digest(b"runtime")),
            },
        }];
        let stop = AtomicBool::new(false);
        extract_runtime(&archive, dir.path(), &cache, &files, &stop, &stop).unwrap();
        assert_eq!(
            fs::read(dir.path().join("runtime.dll")).unwrap(),
            b"runtime"
        );
        assert!(!dir.path().join("untrusted.dll").exists());
        files[0].artifact.sha256 = "00".repeat(32);
        assert!(extract_runtime(&archive, dir.path(), &cache, &files, &stop, &stop).is_err());
        assert_eq!(
            fs::read(dir.path().join("runtime.dll")).unwrap(),
            b"runtime"
        );
        assert!(!cache.join("runtime-0.install").exists());
    }
}
