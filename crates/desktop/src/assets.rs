use anyhow::{Context, Result, ensure};
use cs_core::AppConfig;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy)]
pub struct Language {
    pub code: &'static str,
    pub name: &'static str,
}

// Human-readable choices are filtered against the installed model's vocabulary.
const LANGUAGES: &[(&str, &str)] = &[
    ("af", "Afrikaans"),
    ("sq", "Albanian"),
    ("am", "Amharic"),
    ("ar", "Arabic"),
    ("hy", "Armenian"),
    ("ast", "Asturian"),
    ("az", "Azerbaijani"),
    ("ba", "Bashkir"),
    ("be", "Belarusian"),
    ("bn", "Bengali"),
    ("bs", "Bosnian"),
    ("br", "Breton"),
    ("bg", "Bulgarian"),
    ("my", "Burmese"),
    ("ca", "Catalan"),
    ("ceb", "Cebuano"),
    ("zh", "Chinese"),
    ("hr", "Croatian"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("nl", "Dutch"),
    ("en", "English"),
    ("et", "Estonian"),
    ("ff", "Fulah"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("fy", "Frisian"),
    ("gl", "Galician"),
    ("lg", "Ganda"),
    ("ka", "Georgian"),
    ("de", "German"),
    ("el", "Greek"),
    ("gu", "Gujarati"),
    ("ht", "Haitian Creole"),
    ("ha", "Hausa"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hu", "Hungarian"),
    ("is", "Icelandic"),
    ("ig", "Igbo"),
    ("ilo", "Ilocano"),
    ("id", "Indonesian"),
    ("ga", "Irish"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("jv", "Javanese"),
    ("kn", "Kannada"),
    ("kk", "Kazakh"),
    ("km", "Khmer"),
    ("ko", "Korean"),
    ("lo", "Lao"),
    ("lv", "Latvian"),
    ("ln", "Lingala"),
    ("lt", "Lithuanian"),
    ("lb", "Luxembourgish"),
    ("mk", "Macedonian"),
    ("mg", "Malagasy"),
    ("ms", "Malay"),
    ("ml", "Malayalam"),
    ("mr", "Marathi"),
    ("mn", "Mongolian"),
    ("ne", "Nepali"),
    ("ns", "Northern Sotho"),
    ("no", "Norwegian"),
    ("oc", "Occitan"),
    ("or", "Odia"),
    ("ps", "Pashto"),
    ("fa", "Persian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("pa", "Punjabi"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("gd", "Scottish Gaelic"),
    ("sr", "Serbian"),
    ("sd", "Sindhi"),
    ("si", "Sinhala"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("so", "Somali"),
    ("es", "Spanish"),
    ("su", "Sundanese"),
    ("sw", "Swahili"),
    ("ss", "Swati"),
    ("sv", "Swedish"),
    ("tl", "Tagalog"),
    ("ta", "Tamil"),
    ("th", "Thai"),
    ("tn", "Tswana"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("uz", "Uzbek"),
    ("vi", "Vietnamese"),
    ("cy", "Welsh"),
    ("wo", "Wolof"),
    ("xh", "Xhosa"),
    ("yi", "Yiddish"),
    ("yo", "Yoruba"),
    ("zu", "Zulu"),
];

fn require_file(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| {
        format!(
            "Model file is missing: {}. Open model setup to download it.",
            path.display()
        )
    })?;
    ensure!(
        metadata.is_file() && metadata.len() != 0,
        "Model file is empty or invalid: {}",
        path.display()
    );
    Ok(())
}

pub fn prepare(config: &AppConfig) -> Result<Vec<Language>> {
    if config.voice_enabled {
        require_file(&config.asr_manifest)?;
        let manifest: toml::Value = toml::from_str(&fs::read_to_string(&config.asr_manifest)?)?;
        let files = manifest
            .get("files")
            .and_then(toml::Value::as_table)
            .context("ASR manifest has no model files")?;
        ensure!(!files.is_empty(), "ASR manifest has no model files");
        let base = config.asr_manifest.parent().unwrap_or(Path::new("."));
        for value in files.values() {
            let path = base.join(value.as_str().context("ASR model path must be a string")?);
            if path.is_dir() {
                ensure!(
                    fs::read_dir(&path)?.next().is_some(),
                    "ASR tokenizer directory is empty: {}",
                    path.display()
                );
            } else {
                require_file(&path)?;
            }
        }
    }
    if config.voice_enabled && config.hud_enabled {
        prepare_ocr(&config.ocr_config)?;
    }
    let mt = &config.translation_model;
    require_file(&mt.join("model.bin"))?;
    require_file(&mt.join("config.json"))?;
    require_file(&mt.join("translation.toml"))?;
    let translation: toml::Value =
        toml::from_str(&fs::read_to_string(mt.join("translation.toml"))?)?;
    for key in ["sentencepiece", "vocabulary", "language_identification"] {
        require_file(
            &mt.join(
                translation
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .with_context(|| format!("Translation manifest is missing {key}"))?,
            ),
        )?;
    }
    let vocabulary = mt.join(
        translation["vocabulary"]
            .as_str()
            .context("Invalid vocabulary path")?,
    );
    let supported = cs_inference::supported_translation_languages(&vocabulary)?;
    let languages: Vec<_> = LANGUAGES
        .iter()
        .filter(|(code, _)| supported.contains(*code))
        .map(|&(code, name)| Language { code, name })
        .collect();
    ensure!(
        !languages.is_empty(),
        "Installed translation model has no supported output languages"
    );
    Ok(languages)
}

fn prepare_ocr(path: &Path) -> Result<()> {
    require_file(path)?;
    let config: toml::Value = toml::from_str(&fs::read_to_string(path)?)?;
    let root = path.parent().unwrap_or(Path::new("."));
    require_file(
        &root.join(
            config
                .get("runtime")
                .and_then(toml::Value::as_str)
                .context("Invalid OCR runtime path")?,
        ),
    )?;
    let recognizers = config
        .get("recognizers")
        .and_then(toml::Value::as_array)
        .context("OCR configuration has no recognizers")?;
    ensure!(
        !recognizers.is_empty(),
        "OCR configuration has no recognizers"
    );
    for recognizer in recognizers {
        for key in ["model", "dictionary"] {
            require_file(
                &root.join(
                    recognizer
                        .get(key)
                        .and_then(toml::Value::as_str)
                        .with_context(|| format!("Invalid OCR {key} path"))?,
                ),
            )?;
        }
    }
    if let Some(icon) = config.get("speaker_icon").and_then(toml::Value::as_str)
        && !icon.is_empty()
    {
        require_file(&root.join(icon))?;
    }
    Ok(())
}

pub fn root_for_executable(executable: &Path) -> PathBuf {
    executable.parent().unwrap_or(Path::new(".")).to_owned()
}

pub fn resolve(config: &mut AppConfig, base: &Path) {
    for path in [
        &mut config.asr_manifest,
        &mut config.translation_model,
        &mut config.ocr_config,
    ] {
        if path.is_relative() {
            *path = base.join(&*path);
        }
    }
    if let Some(path) = &mut config.console_log
        && path.is_relative()
    {
        *path = base.join(&*path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolving_assets_preserves_absolute_paths() {
        let base = std::env::temp_dir().join("cs-translator-assets");
        let absolute = base.join("translation");
        let mut config = AppConfig {
            asr_manifest: PathBuf::from("models/asr.toml"),
            translation_model: absolute.clone(),
            ..AppConfig::default()
        };
        resolve(&mut config, &base);
        assert_eq!(config.asr_manifest, base.join("models/asr.toml"));
        assert_eq!(config.translation_model, absolute);
    }

    #[test]
    fn ocr_preparation_uses_custom_relative_and_absolute_assets_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("custom.onnx");
        fs::write(&model, b"custom model").unwrap();
        fs::write(dir.path().join("runtime.dll"), b"runtime").unwrap();
        fs::write(dir.path().join("dictionary.txt"), b"dictionary").unwrap();
        let config = format!(
            "runtime = 'runtime.dll'\n[[recognizers]]\nmodel = '{}'\ndictionary = 'dictionary.txt'\n",
            model.display(),
        );
        let path = dir.path().join("ocr.toml");
        fs::write(&path, &config).unwrap();
        prepare_ocr(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), config);
        let explicit_icon = format!("speaker_icon = 'missing-icon.png'\n{config}");
        fs::write(&path, explicit_icon).unwrap();
        assert!(prepare_ocr(&path).is_err());
        fs::write(dir.path().join("missing-icon.png"), b"custom icon").unwrap();
        prepare_ocr(&path).unwrap();
        fs::remove_file(dir.path().join("dictionary.txt")).unwrap();
        assert!(prepare_ocr(&path).is_err());
    }
}
