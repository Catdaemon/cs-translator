use anyhow::{Context, Result, ensure};
use cs_platform::HudOcr;
use std::path::Path;

#[test]
#[ignore = "Requires CS_TRANSLATOR_TEST_OCR_CONFIG and CS_TRANSLATOR_TEST_OCR_IMAGE"]
fn repairing_failed_runtime_preserves_recognition() -> Result<()> {
    if let Some(directory) = std::env::var_os("CS_TRANSLATOR_OCR_TEST_CHILD") {
        return child(Path::new(&directory));
    }
    let temporary = tempfile::tempdir()?;
    let mut observations = Vec::new();
    for mode in ["healthy", "repair"] {
        let directory = temporary.path().join(mode);
        std::fs::create_dir(&directory)?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "repairing_failed_runtime_preserves_recognition",
                "--nocapture",
            ])
            .env("CS_TRANSLATOR_OCR_TEST_CHILD", &directory)
            .env("CS_TRANSLATOR_OCR_TEST_MODE", mode)
            .output()?;
        ensure!(
            output.status.success(),
            "OCR {mode} child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout)?;
        observations.push(
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("OCR_OBSERVATION="))
                .context("Missing native OCR observation")?
                .to_owned(),
        );
    }
    assert_eq!(
        observations[0], observations[1],
        "Repairing a failed runtime must retain the healthy runtime's recognized labels and truncation"
    );
    Ok(())
}

fn child(directory: &Path) -> Result<()> {
    let source = std::path::PathBuf::from(
        std::env::var_os("CS_TRANSLATOR_TEST_OCR_CONFIG").context("OCR config")?,
    );
    let root = source.parent().context("OCR config parent")?;
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(&source)?)?;
    let runtime = root.join(config["runtime"].as_str().context("OCR runtime")?);
    // Exercise the shipped standard glyph, without any saved calibration or
    // captured template; custom diagnostic configs must not weaken this proof.
    config["speaker_icon"] = toml::Value::String(String::new());
    for recognizer in config["recognizers"]
        .as_array_mut()
        .context("OCR recognizers")?
    {
        for key in ["model", "dictionary"] {
            recognizer[key] = toml::Value::String(
                root.join(recognizer[key].as_str().context("recognizer path")?)
                    .to_str()
                    .context("UTF-8 recognizer path")?
                    .into(),
            );
        }
    }
    let private_runtime = directory.join("private-ocr-runtime.dll");
    config["runtime"] = toml::Value::String(
        private_runtime
            .to_str()
            .context("UTF-8 runtime path")?
            .into(),
    );
    let configured = directory.join("ocr.toml");
    std::fs::write(&configured, toml::to_string(&config)?)?;
    if std::env::var("CS_TRANSLATOR_OCR_TEST_MODE")? == "repair" {
        std::fs::write(&private_runtime, b"invalid native runtime")?;
        ensure!(
            HudOcr::load(&configured).is_err(),
            "Invalid runtime must return a recoverable initialization error"
        );
    }
    std::fs::copy(runtime, private_runtime)?;
    let mut ocr = HudOcr::load(&configured)?;
    let image =
        image::open(std::env::var_os("CS_TRANSLATOR_TEST_OCR_IMAGE").context("OCR image")?)?
            .to_rgba8();
    let (x, y, width, height) = ocr.region().pixels(image.width(), image.height())?;
    let region = image::imageops::crop_imm(&image, x, y, width, height).to_image();
    let observation = ocr.recognize(&region, 125)?;
    if let Ok(expected) = std::env::var("CS_TRANSLATOR_TEST_OCR_EXPECTED_NAME") {
        ensure!(
            observation
                .players
                .iter()
                .any(|p| p.text == expected && p.truncated),
            "Expected conservatively bounded standard-icon label {expected:?}, got {observation:?}"
        );
    }
    println!("OCR_OBSERVATION={observation:?}");
    let blank = image::RgbaImage::from_pixel(width, height, image::Rgba([32, 32, 32, 255]));
    ensure!(
        ocr.recognize(&blank, 250)?.players.is_empty(),
        "Inactive frame produced a speaker"
    );
    if let Some(paths) = std::env::var_os("CS_TRANSLATOR_TEST_OCR_INACTIVE_IMAGES") {
        for path in std::env::split_paths(&paths) {
            let inactive = image::open(&path)?.to_rgba8();
            let (x, y, width, height) = ocr.region().pixels(inactive.width(), inactive.height())?;
            let crop = image::imageops::crop_imm(&inactive, x, y, width, height).to_image();
            ensure!(
                ocr.recognize(&crop, 375)?.players.is_empty(),
                "Recorded inactive control produced a speaker: {}",
                path.display()
            );
        }
    }
    if let Ok(expected) = std::env::var("CS_TRANSLATOR_TEST_OCR_EXPECTED_NAME") {
        ocr.invalidate_layout();
        ocr.recognize(&region, 1000)?;
        ocr.recognize(&blank, 1125)?;
        let mut shifted = blank.clone();
        image::imageops::overlay(&mut shifted, &region, 200, 0);
        ensure!(
            ocr.recognize(&shifted, 1250)?.players.is_empty(),
            "A moved column must not retain stale speaker labels during search backoff"
        );
        let reacquired = ocr.recognize(&shifted, 3250)?;
        ensure!(
            reacquired
                .players
                .iter()
                .any(|player| player.text == expected),
            "An unchanged frame must be searched again after reacquisition backoff: {reacquired:?}"
        );
    }
    Ok(())
}
