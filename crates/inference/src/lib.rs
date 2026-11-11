use anyhow::{Context, Result, bail, ensure};
use cs_core::{AudioChunk, Recognition};
use serde::Deserialize;
use sherpa_onnx::{
    LinearResampler, OfflineRecognizer, OfflineRecognizerConfig, OnlineRecognizer,
    OnlineRecognizerConfig, OnlineStream,
};
use std::{borrow::Cow, collections::BTreeMap, fs, path::Path};

mod translation;
pub use translation::{Translator, supported_translation_languages};

#[derive(Deserialize)]
struct Manifest {
    name: String,
    engine: String,
    files: BTreeMap<String, String>,
    #[serde(default = "phrase_seconds")]
    max_phrase_seconds: f32,
    #[serde(default = "silence_seconds")]
    silence_seconds: f32,
    #[serde(default = "speech_threshold")]
    speech_rms_threshold: f32,
}
fn phrase_seconds() -> f32 {
    8.0
}
fn silence_seconds() -> f32 {
    0.5
}
fn speech_threshold() -> f32 {
    0.008
}

enum Engine {
    Online {
        stream: OnlineStream,
        recognizer: OnlineRecognizer,
    },
    Offline(OfflineRecognizer),
}

/// Persistent CPU model. Offline models use bounded energy-endpointed phrases;
/// Nemotron retains the native encoder/decoder caches between audio chunks.
pub struct SpeechRecognizer {
    engine: Engine,
    language: String,
    samples: Vec<f32>,
    start_ms: Option<u64>,
    end_ms: u64,
    silence_samples: usize,
    max_samples: usize,
    silence_limit: usize,
    threshold: f32,
    last_partial: String,
    resampler: Option<LinearResampler>,
    last_input_end_ms: Option<u64>,
    input_sample_rate: Option<u32>,
    stream_origin_ms: Option<u64>,
    final_end_ms: u64,
}
impl SpeechRecognizer {
    pub fn load(
        manifest_path: &Path,
        threads: usize,
        language: &str,
        beam_search: bool,
    ) -> Result<Self> {
        ensure!(
            threads > 0 && threads <= i32::MAX as usize,
            "threads must be positive"
        );
        ensure!(!language.contains('\0'), "invalid source language");
        let m: Manifest = toml::from_str(
            &fs::read_to_string(manifest_path)
                .with_context(|| format!("Read model manifest {}", manifest_path.display()))?,
        )?;
        ensure!(
            m.max_phrase_seconds.is_finite() && (1.0..=25.0).contains(&m.max_phrase_seconds),
            "max_phrase_seconds must be 1..25"
        );
        ensure!(
            m.silence_seconds.is_finite() && (0.1..=3.0).contains(&m.silence_seconds),
            "silence_seconds must be 0.1..3"
        );
        ensure!(
            !beam_search || m.engine == "nemotron",
            "Speech beam search requires the streaming Nemotron model"
        );
        if m.engine != "nemotron" {
            ensure!(
                m.speech_rms_threshold.is_finite() && (0.0..1.0).contains(&m.speech_rms_threshold),
                "invalid speech RMS threshold"
            );
        }
        let file = |key: &str| -> Result<String> {
            let value = m
                .files
                .get(key)
                .with_context(|| format!("{} missing files.{key}", m.name))?;
            let path = manifest_path.parent().unwrap_or(Path::new(".")).join(value);
            ensure!(
                path.exists(),
                "Missing model asset {}. Download speech models in the app's setup screen.",
                path.display()
            );
            let s = path.to_str().context("Model path must be Unicode")?;
            ensure!(!s.contains('\0'), "invalid model path");
            Ok(s.to_owned())
        };
        let language = if language.is_empty() {
            "auto"
        } else {
            language
        }
        .to_owned();
        let engine = if m.engine == "nemotron" {
            let mut c = OnlineRecognizerConfig::default();
            c.model_config.transducer.encoder = Some(file("encoder")?);
            c.model_config.transducer.decoder = Some(file("decoder")?);
            c.model_config.transducer.joiner = Some(file("joiner")?);
            c.model_config.tokens = Some(file("tokens")?);
            c.model_config.num_threads = threads as i32;
            c.model_config.provider = Some("cpu".into());
            c.decoding_method = Some(
                if beam_search {
                    "modified_beam_search"
                } else {
                    "greedy_search"
                }
                .into(),
            );
            c.max_active_paths = 4;
            c.enable_endpoint = true;
            c.rule2_min_trailing_silence = m.silence_seconds;
            // Rule 3 includes leading idle. Enforce the phrase limit below using
            // decoded token time instead; native silence endpoints bound idle.
            c.rule3_min_utterance_length = 1_000_000.0;
            let recognizer = OnlineRecognizer::create(&c).context(
                "Native streaming model initialization failed; check manifest and runtime DLLs",
            )?;
            let stream = recognizer.create_stream();
            stream.set_option("language", &language);
            Engine::Online { stream, recognizer }
        } else {
            let mut c = OfflineRecognizerConfig::default();
            c.model_config.num_threads = threads as i32;
            c.model_config.provider = Some("cpu".into());
            match m.engine.as_str() {
                "whisper" => {
                    c.model_config.whisper.encoder = Some(file("encoder")?);
                    c.model_config.whisper.decoder = Some(file("decoder")?);
                    c.model_config.whisper.language = Some(if language == "auto" {
                        String::new()
                    } else {
                        language.clone()
                    });
                    c.model_config.whisper.task = Some("transcribe".into());
                    c.model_config.tokens = Some(file("tokens")?);
                }
                "parakeet" => {
                    c.model_config.transducer.encoder = Some(file("encoder")?);
                    c.model_config.transducer.decoder = Some(file("decoder")?);
                    c.model_config.transducer.joiner = Some(file("joiner")?);
                    c.model_config.tokens = Some(file("tokens")?);
                    c.model_config.model_type = Some("nemo_transducer".into());
                }
                "qwen" => {
                    c.model_config.qwen3_asr.conv_frontend = Some(file("conv_frontend")?);
                    c.model_config.qwen3_asr.encoder = Some(file("encoder")?);
                    c.model_config.qwen3_asr.decoder = Some(file("decoder")?);
                    c.model_config.qwen3_asr.tokenizer = Some(file("tokenizer")?);
                    c.model_config.tokens = Some(String::new());
                }
                other => bail!("Unsupported ASR engine {other}"),
            }
            Engine::Offline(OfflineRecognizer::create(&c).context(
                "Native offline model initialization failed; check manifest and runtime DLLs",
            )?)
        };
        Ok(Self {
            engine,
            language,
            samples: Vec::with_capacity(if m.engine == "nemotron" {
                0
            } else {
                (16000.0 * m.max_phrase_seconds) as usize
            }),
            start_ms: None,
            end_ms: 0,
            silence_samples: 0,
            max_samples: (16000.0 * m.max_phrase_seconds) as usize,
            silence_limit: (16000.0 * m.silence_seconds) as usize,
            threshold: m.speech_rms_threshold,
            last_partial: String::new(),
            resampler: None,
            last_input_end_ms: None,
            input_sample_rate: None,
            stream_origin_ms: None,
            final_end_ms: 0,
        })
    }
    pub fn push(&mut self, chunk: &AudioChunk) -> Result<Vec<Recognition>> {
        self.push_audio(
            &chunk.samples,
            chunk.sample_rate,
            chunk.timestamp_ms,
            chunk.discontinuity,
        )
    }
    fn push_audio(
        &mut self,
        samples: &[f32],
        sample_rate: u32,
        timestamp_ms: u64,
        discontinuity: bool,
    ) -> Result<Vec<Recognition>> {
        validate_audio(samples, sample_rate)?;
        let gap = discontinuity
            || self
                .input_sample_rate
                .is_some_and(|rate| rate != sample_rate)
            || self
                .last_input_end_ms
                .is_some_and(|expected| timestamp_ms.abs_diff(expected) > 3);
        let mut previous = if gap { self.finish()? } else { Vec::new() };
        if samples.is_empty() {
            return Ok(previous);
        }
        self.last_input_end_ms =
            Some(timestamp_ms + samples.len() as u64 * 1000 / u64::from(sample_rate));
        self.input_sample_rate = Some(sample_rate);
        let current = self.push_contiguous(samples, sample_rate, timestamp_ms)?;
        if previous.is_empty() {
            return Ok(current);
        }
        previous.extend(current);
        Ok(previous)
    }
    fn push_contiguous(
        &mut self,
        samples: &[f32],
        sample_rate: u32,
        timestamp_ms: u64,
    ) -> Result<Vec<Recognition>> {
        // Native sherpa performs sample-rate conversion for streaming. Offline
        // endpointing uses 16 kHz to keep all limits independent of capture rate.
        if let Engine::Online { stream, recognizer } = &self.engine {
            self.stream_origin_ms.get_or_insert(timestamp_ms);
            self.start_ms.get_or_insert(timestamp_ms);
            self.end_ms = timestamp_ms + samples.len() as u64 * 1000 / u64::from(sample_rate);
            stream.accept_waveform(sample_rate as i32, samples);
            while recognizer.is_ready(stream) {
                recognizer.decode(stream);
            }
            let result = recognizer
                .get_result(stream)
                .context("Streaming decoder returned no result")?;
            let endpoint = recognizer.is_endpoint(stream);
            self.update_online_bounds(result.start_time, result.timestamps.as_deref());
            let final_result = endpoint
                || (!result.text.trim().is_empty()
                    && result.timestamps.as_ref().is_some_and(|times| {
                        match (times.first(), times.last()) {
                            (Some(first), Some(last)) => {
                                last - first >= self.max_samples as f32 / 16000.0
                            }
                            _ => false,
                        }
                    }));
            let changed = result.text != self.last_partial;
            self.last_partial.clone_from(&result.text);
            let output = if !result.text.trim().is_empty() && (changed || final_result) {
                vec![self.recognition(result.text, final_result)]
            } else {
                Vec::new()
            };
            if final_result {
                if let Engine::Online { stream, recognizer } = &self.engine {
                    recognizer.reset(stream);
                    stream.set_option("language", &self.language);
                }
                // Reset preserves undecoded feature frames; finish must still drain them.
                if !output.is_empty() {
                    self.final_end_ms = self.end_ms;
                }
                self.start_ms = None;
                self.last_partial.clear();
            }
            return Ok(output);
        }
        let audio = if sample_rate == 16000 {
            ensure!(
                self.resampler.is_none(),
                "Finish audio before changing capture sample rate"
            );
            Cow::Borrowed(samples)
        } else {
            if let Some(resampler) = &self.resampler {
                ensure!(
                    resampler.input_sample_rate() == sample_rate as i32,
                    "Finish audio before changing capture sample rate"
                );
            } else {
                self.resampler = Some(
                    LinearResampler::create(sample_rate as i32, 16000)
                        .context("Create band-limited audio resampler")?,
                );
            }
            Cow::Owned(
                self.resampler
                    .as_ref()
                    .context("Resampler not initialized")?
                    .resample(samples, false),
            )
        };
        self.process_offline(&audio, timestamp_ms)
    }
    fn process_offline(&mut self, audio: &[f32], timestamp_ms: u64) -> Result<Vec<Recognition>> {
        let mut output = Vec::new();
        for (index, frame) in audio.chunks(320).enumerate() {
            let rms = (frame.iter().map(|v| v * v).sum::<f32>() / frame.len() as f32).sqrt();
            let speech = rms >= self.threshold;
            if self.start_ms.is_none() && !speech {
                continue;
            }
            self.start_ms
                .get_or_insert(timestamp_ms + index as u64 * 20);
            self.samples.extend_from_slice(frame);
            self.end_ms = timestamp_ms + (index * 320 + frame.len()) as u64 * 1000 / 16000;
            self.silence_samples = if speech {
                0
            } else {
                self.silence_samples + frame.len()
            };
            if self.samples.len() >= self.max_samples || self.silence_samples >= self.silence_limit
            {
                output.extend(self.flush_offline()?);
            }
        }
        Ok(output)
    }
    fn update_online_bounds(&mut self, start: Option<f32>, timestamps: Option<&[f32]>) {
        // Native start_time remains relative to the stream origin across resets.
        // A token occupies at most one 80 ms encoder step at the right edge.
        if let (Some(origin), Some(times)) = (self.stream_origin_ms, timestamps)
            && let (Some(first), Some(last)) = (times.first(), times.last())
        {
            let base = start.unwrap_or(0.0);
            if base.is_finite() && first.is_finite() && last.is_finite() {
                let begin = origin.saturating_add(((base + first).max(0.0) * 1000.0) as u64);
                let end = origin
                    .saturating_add(((base + last).max(0.0) * 1000.0) as u64)
                    .saturating_add(80);
                self.start_ms = Some(begin.max(self.final_end_ms));
                self.end_ms = end
                    .min(self.last_input_end_ms.unwrap_or(end))
                    .max(self.start_ms.unwrap_or(0));
            }
        }
        // Models without token times retain the accepted-audio interval fallback.
    }
    fn recognition(&self, text: String, is_final: bool) -> Recognition {
        let language = if self.language == "auto" {
            None
        } else {
            Some(self.language.clone())
        };
        Recognition {
            text: text.trim().to_owned(),
            language,
            start_ms: self.start_ms.unwrap_or(0),
            end_ms: self.end_ms,
            is_final,
        }
    }
    fn flush_offline(&mut self) -> Result<Vec<Recognition>> {
        if self.samples.is_empty() {
            return Ok(Vec::new());
        }
        let Engine::Offline(recognizer) = &self.engine else {
            bail!("Offline flush on streaming model")
        };
        let stream = recognizer.create_stream();
        if self.language != "auto" {
            stream.set_option("language", &self.language);
        }
        stream.accept_waveform(16000, &self.samples);
        recognizer.decode(&stream);
        let text = stream
            .get_result()
            .context("Offline decoder returned no result")?
            .text;
        let output = if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![self.recognition(text, true)]
        };
        self.samples.clear();
        self.start_ms = None;
        self.silence_samples = 0;
        Ok(output)
    }
    pub fn finish(&mut self) -> Result<Vec<Recognition>> {
        let input_end_ms = self.last_input_end_ms.take().unwrap_or(self.end_ms);
        let input_sample_rate = self.input_sample_rate.take();
        match &self.engine {
            Engine::Offline(_) => {
                let mut output = Vec::new();
                if let Some(resampler) = self.resampler.take() {
                    let tail = resampler.resample(&[], true);
                    let tail_start = input_end_ms.saturating_sub(tail.len() as u64 * 1000 / 16000);
                    output.extend(self.process_offline(&tail, tail_start)?);
                }
                output.extend(self.flush_offline()?);
                self.last_input_end_ms = None;
                Ok(output)
            }
            Engine::Online { stream, recognizer } => {
                let Some(sample_rate) = input_sample_rate else {
                    return Ok(Vec::new());
                };
                self.last_input_end_ms = Some(input_end_ms);
                // NeMo's 560 ms encoder advances over 650 ms windows. Half a second
                // can leave the last word buffered at EOF; one second also covers
                // delayed token emission across the next encoder step.
                // Keep the original input rate and cap output times to real audio.
                self.samples.resize(sample_rate as usize, 0.0);
                stream.accept_waveform(sample_rate as i32, &self.samples);
                self.samples.clear();
                stream.input_finished();
                while recognizer.is_ready(stream) {
                    recognizer.decode(stream);
                }
                let result = recognizer
                    .get_result(stream)
                    .context("Streaming flush returned no result")?;
                let fresh = recognizer.create_stream();
                fresh.set_option("language", &self.language);
                self.update_online_bounds(result.start_time, result.timestamps.as_deref());
                let text = result.text;
                let output = if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![self.recognition(text, true)]
                };
                if let Engine::Online { stream, .. } = &mut self.engine {
                    *stream = fresh;
                }
                self.start_ms = None;
                self.stream_origin_ms = None;
                self.final_end_ms = 0;
                self.last_input_end_ms = None;
                self.last_partial.clear();
                Ok(output)
            }
        }
    }
    pub fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<Recognition> {
        validate_audio(samples, sample_rate)?;
        ensure!(
            self.start_ms.is_none(),
            "Finish pending streaming audio before transcribe"
        );
        let mut results = Vec::new();
        // Bounded calls prevent native stream lengths from growing with a media file.
        for (i, part) in samples.chunks(sample_rate as usize).enumerate() {
            results.extend(self.push_audio(part, sample_rate, i as u64 * 1000, false)?);
        }
        results.extend(self.finish()?);
        let finals: Vec<_> = results.into_iter().filter(|r| r.is_final).collect();
        let text = finals
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let language = if self.language == "auto" {
            None
        } else {
            Some(self.language.clone())
        };
        Ok(Recognition {
            text,
            language,
            start_ms: 0,
            end_ms: samples.len() as u64 * 1000 / u64::from(sample_rate),
            is_final: true,
        })
    }
}
fn validate_audio(samples: &[f32], rate: u32) -> Result<()> {
    ensure!(
        (8000..=192000).contains(&rate),
        "Unsupported sample rate {rate}"
    );
    ensure!(
        samples.iter().all(|s| s.is_finite()),
        "Audio contains nonfinite samples"
    );
    Ok(())
}
pub fn read_wav(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("Read WAV {}", path.display()))?;
    let spec = reader.spec();
    ensure!(spec.channels > 0, "WAV has no channels");
    validate_audio(&[], spec.sample_rate)?;
    let mut interleaved = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            ensure!(
                (1..=32).contains(&spec.bits_per_sample),
                "Unsupported PCM bit depth"
            );
            let scale = 2.0_f32.powi(i32::from(spec.bits_per_sample) - 1);
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<std::result::Result<Vec<_>, _>>()?
        }
    };
    ensure!(
        interleaved.len() % usize::from(spec.channels) == 0,
        "Incomplete WAV frame"
    );
    let channels = usize::from(spec.channels);
    if channels > 1 {
        let frames = interleaved.len() / channels;
        for index in 0..frames {
            interleaved[index] = interleaved[index * channels..(index + 1) * channels]
                .iter()
                .sum::<f32>()
                / channels as f32;
        }
        interleaved.truncate(frames);
    }
    validate_audio(&interleaved, spec.sample_rate)?;
    Ok((interleaved, spec.sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn temporary(extension: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cs-inference-{}-{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            extension
        ))
    }
    #[test]
    fn wav_downmixes_interleaved_pcm_without_changing_duration() -> Result<()> {
        let path = temporary("wav");
        {
            let mut writer = hound::WavWriter::create(
                &path,
                hound::WavSpec {
                    channels: 2,
                    sample_rate: 16000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )?;
            for sample in [16384_i16, -16384, 8192, 8192] {
                writer.write_sample(sample)?;
            }
            writer.finalize()?;
        }
        let decoded = read_wav(&path);
        fs::remove_file(path)?;
        let (samples, rate) = decoded?;
        assert_eq!(rate, 16000);
        assert_eq!(samples, [0.0, 0.25]);
        Ok(())
    }
    #[test]
    fn rejects_invalid_audio_and_nonfinite_values() {
        assert!(validate_audio(&[f32::NAN], 16000).is_err());
        assert!(validate_audio(&[0.0], 0).is_err());
        assert!(validate_audio(&[], 48000).is_ok());
    }
}
