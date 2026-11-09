use anyhow::{Context, Result, ensure};
use ct2rs::{
    ComputeType, Config, Device,
    sys::{TranslationOptions, Translator as NativeTranslator},
};
use sentencepiece::SentencePieceProcessor;
use serde::{
    Deserialize,
    de::{SeqAccess, Visitor},
};
use std::{borrow::Cow, collections::HashSet, fmt, fs, io::BufReader, path::Path};

fn language_tag(token: &str) -> Option<&str> {
    let code = token.strip_prefix("__")?.strip_suffix("__")?;
    ((2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase())).then_some(code)
}

/// Read only language capabilities, without retaining the model's full vocabulary.
pub fn supported_translation_languages(path: &Path) -> Result<HashSet<String>> {
    struct Languages;
    impl<'de> Visitor<'de> for Languages {
        type Value = HashSet<String>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a vocabulary array")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut languages = HashSet::new();
            while let Some(token) = seq.next_element::<String>()? {
                if let Some(code) = language_tag(&token) {
                    languages.insert(code.to_owned());
                }
            }
            Ok(languages)
        }
    }
    let mut decoder = serde_json::Deserializer::from_reader(BufReader::new(fs::File::open(path)?));
    let result = serde::de::Deserializer::deserialize_seq(&mut decoder, Languages)?;
    decoder.end()?;
    Ok(result)
}

#[derive(Deserialize)]
struct TranslationManifest {
    architecture: String,
    sentencepiece: String,
    vocabulary: String,
    language_identification: String,
}
/// Persistent CTranslate2 INT8 CPU model, using the original M2M100
/// SentencePiece vocabulary and language-prefix protocol (not NLLB tokens).
pub struct Translator {
    native: NativeTranslator,
    tokenizer: SentencePieceProcessor,
    vocabulary: HashSet<String>,
    language_identifier: fasttext::FastText,
    beam_size: usize,
}
impl Translator {
    pub fn load(model_dir: &Path, threads: usize, beam_size: usize) -> Result<Self> {
        ensure!(threads > 0, "Translation thread count must be positive");
        ensure!(
            (1..=2).contains(&beam_size),
            "Translation beam size must be 1 or 2"
        );
        let config: TranslationManifest = toml::from_str(
            &fs::read_to_string(model_dir.join("translation.toml")).context(
                "Missing translation.toml: download translation models in the app's setup screen",
            )?,
        )?;
        ensure!(
            config.architecture == "m2m100",
            "Unsupported translation architecture {}",
            config.architecture
        );
        let vocabulary = supported_translation_languages(&model_dir.join(config.vocabulary))?;
        let tokenizer = SentencePieceProcessor::open(model_dir.join(config.sentencepiece))
            .context("Load M2M100 SentencePiece model")?;
        let language_identifier =
            fasttext::FastText::load_model(model_dir.join(config.language_identification))
                .context("Load local fastText language model: download language identification in the app's setup screen")?;
        let native = NativeTranslator::new(
            model_dir,
            &Config {
                device: Device::CPU,
                compute_type: ComputeType::INT8,
                num_threads_per_replica: threads,
                max_queued_batches: 1,
                ..Default::default()
            },
        )
        .context("Load CTranslate2 M2M100 model")?;
        Ok(Self {
            native,
            tokenizer,
            vocabulary,
            language_identifier,
            beam_size,
        })
    }
    pub fn translate(&mut self, text: &str, source: &str, target: &str) -> Result<String> {
        if text.trim().is_empty() {
            return Ok(String::new());
        }
        let detected;
        let source = if source.is_empty() || source == "auto" {
            let detection_text = if text.contains(['\r', '\n']) {
                Cow::Owned(text.replace(['\r', '\n'], " "))
            } else {
                Cow::Borrowed(text)
            };
            detected = self
                .language_identifier
                .predict(&detection_text, 1, 0.0)
                .into_iter()
                .next()
                .and_then(|prediction| {
                    prediction
                        .label
                        .strip_prefix("__label__")
                        .map(str::to_owned)
                })
                .context("Cannot identify source language; select it explicitly")?;
            detected.as_str()
        } else {
            source
        };
        let source = cs_core::normalize_language(source)?;
        let target = cs_core::normalize_language(target)?;
        let source_token = format!("__{source}__");
        let target_token = format!("__{target}__");
        ensure!(
            self.vocabulary.contains(&source),
            "M2M100 does not support source language {source}"
        );
        ensure!(
            self.vocabulary.contains(&target),
            "M2M100 does not support target language {target}"
        );
        if source == target {
            return Ok(text.to_owned());
        }
        let pieces = self
            .tokenizer
            .encode(text)
            .context("Tokenize source text")?;
        // Prefer complete sentences, then whole words. Only an individual
        // overlong word needs a hard token boundary.
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < pieces.len() {
            let limit = (offset + 448).min(pieces.len());
            let end = if limit == pieces.len() {
                limit
            } else {
                (offset + 1..=limit)
                    .rev()
                    .find(|&i| {
                        pieces[i - 1]
                            .piece
                            .ends_with(['.', '!', '?', '。', '！', '？'])
                    })
                    .or_else(|| {
                        (offset + 1..=limit)
                            .rev()
                            .find(|&i| pieces[i].piece.starts_with('▁'))
                    })
                    .unwrap_or(limit)
            };
            let part = &pieces[offset..end];
            offset = end;
            let mut input = Vec::with_capacity(part.len() + 2);
            input.push(source_token.clone());
            input.extend(part.iter().map(|piece| piece.piece.clone()));
            input.push("</s>".into());
            let options: TranslationOptions<String, String> = TranslationOptions {
                beam_size: self.beam_size,
                max_input_length: 512,
                max_decoding_length: 512,
                return_end_token: true,
                ..Default::default()
            };
            let results = self.native.translate_batch_with_target_prefix(
                &[input],
                &[vec![target_token.clone()]],
                &options,
                None,
            )?;
            let tokens = results
                .first()
                .and_then(|r| r.hypotheses.first())
                .context("Translator produced no hypothesis")?;
            ensure!(
                tokens.last().is_some_and(|s| s == "</s>"),
                "Translation reached its decoder limit without EOS; shorten the source caption"
            );
            output.extend(
                tokens
                    .iter()
                    .filter(|s| {
                        s.as_str() != "</s>" && s.as_str() != "<s>" && language_tag(s).is_none()
                    })
                    .cloned(),
            );
        }
        let clean: Vec<&str> = output.iter().map(String::as_str).collect();
        let decoded = self.tokenizer.decode_pieces(&clean)?;
        ensure!(
            !decoded.trim().is_empty(),
            "Translator produced an empty translation"
        );
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn vocabulary_capabilities_exclude_literal_underscores_and_invalid_tags() -> anyhow::Result<()>
    {
        let path = std::env::temp_dir().join(format!(
            "cs-language-vocabulary-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"["normal","__","____","________","__en__","__ceb__","__english__","__EN__","__en__"]"#,
        )?;
        let result = super::supported_translation_languages(&path);
        std::fs::remove_file(path)?;
        assert_eq!(
            result?,
            ["en".to_owned(), "ceb".to_owned()].into_iter().collect()
        );
        Ok(())
    }
    #[test]
    fn normalizes_language_locales_without_hardcoding_target() {
        assert_eq!(cs_core::normalize_language("ru-RU").unwrap(), "ru");
        assert_eq!(cs_core::normalize_language("JA_jp").unwrap(), "ja");
        assert_eq!(cs_core::normalize_language("ceb").unwrap(), "ceb");
        assert!(cs_core::normalize_language("auto").is_err());
    }
}
