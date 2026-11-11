use cs_core::AudioChunk;
use cs_inference::{SpeechRecognizer, read_wav};
use std::path::PathBuf;

fn canonical(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

// The model and labelled audio are external so ordinary workspace tests remain offline.
// CS_TRANSLATOR_TEST_ASR: Nemotron manifest; CS_TRANSLATOR_TEST_AUDIO: 48 kHz mono
// recording of "Behind you". The evaluation records the controlled fixture's provenance.
#[test]
#[ignore = "Requires CS_TRANSLATOR_TEST_ASR and CS_TRANSLATOR_TEST_AUDIO (48 kHz 'Behind you')"]
fn short_speech_survives_live_rate_gaps_and_rate_changes() -> anyhow::Result<()> {
    let manifest = PathBuf::from(std::env::var_os("CS_TRANSLATOR_TEST_ASR").expect("ASR manifest"));
    let audio_path =
        PathBuf::from(std::env::var_os("CS_TRANSLATOR_TEST_AUDIO").expect("48 kHz fixture"));
    let (audio, rate) = read_wav(&audio_path)?;
    assert_eq!(rate, 48000, "Exercise the actual live-capture sample rate");
    let mut asr = SpeechRecognizer::load(&manifest, 2, "auto", false)?;
    let mut finals = Vec::new();
    let mut cursor_ms = 0;
    for segment in 0..3 {
        let samples;
        let (waveform, rate) = if segment == 2 {
            samples = audio
                .as_chunks::<3>()
                .0
                .iter()
                .map(|s| (s[0] + s[1] + s[2]) / 3.)
                .collect::<Vec<_>>();
            (samples.as_slice(), 16000)
        } else {
            (audio.as_slice(), 48000)
        };
        if segment == 1 {
            cursor_ms += 100;
        }
        let length = rate as usize / 100;
        for (index, chunk) in waveform.chunks(length).enumerate() {
            let timestamp_ms = cursor_ms + (index * length) as u64 * 1000 / u64::from(rate);
            for result in asr.push(&AudioChunk {
                samples: chunk.to_vec(),
                sample_rate: rate,
                timestamp_ms,
                discontinuity: segment == 1 && index == 0,
            })? {
                if result.is_final {
                    finals.push(result.text);
                }
            }
        }
        cursor_ms += waveform.len() as u64 * 1000 / u64::from(rate);
    }
    finals.extend(
        asr.finish()?
            .into_iter()
            .filter(|r| r.is_final)
            .map(|r| r.text),
    );
    let words = finals
        .iter()
        .map(|text| canonical(text))
        .collect::<Vec<_>>();
    assert_eq!(
        words,
        ["behindyou", "behindyou", "behindyou"],
        "No callout may disappear across the discontinuity, rate change, or final flush: {finals:?}"
    );
    Ok(())
}

#[test]
#[ignore = "Requires CS_TRANSLATOR_TEST_ASR and CS_TRANSLATOR_TEST_AUDIO (48 kHz 'Behind you')"]
fn beam_search_preserves_short_callout_and_media_bounds() -> anyhow::Result<()> {
    let manifest = PathBuf::from(std::env::var_os("CS_TRANSLATOR_TEST_ASR").expect("ASR manifest"));
    let audio_path =
        PathBuf::from(std::env::var_os("CS_TRANSLATOR_TEST_AUDIO").expect("48 kHz fixture"));
    let (audio, rate) = read_wav(&audio_path)?;
    let duration_ms = audio.len() as u64 * 1000 / u64::from(rate);
    let mut asr = SpeechRecognizer::load(&manifest, 2, "auto", true)?;
    let mut finals = Vec::new();
    let length = rate as usize / 100;
    for (index, samples) in audio.chunks(length).enumerate() {
        finals.extend(
            asr.push(&AudioChunk {
                samples: samples.to_vec(),
                sample_rate: rate,
                timestamp_ms: (index * length) as u64 * 1000 / u64::from(rate),
                discontinuity: false,
            })?
            .into_iter()
            .filter(|result| result.is_final),
        );
    }
    finals.extend(asr.finish()?.into_iter().filter(|result| result.is_final));
    assert_eq!(
        finals
            .iter()
            .map(|r| canonical(&r.text))
            .collect::<Vec<_>>(),
        ["behindyou"]
    );
    assert!(
        finals
            .iter()
            .all(|r| r.start_ms < r.end_ms && r.end_ms <= duration_ms)
    );
    Ok(())
}

#[test]
#[ignore = "Requires CS_TRANSLATOR_TEST_ASR and CS_TRANSLATOR_TEST_ANNOUNCER_AUDIO ('Terrorists win' without added trailing silence)"]
fn eof_retains_announcer_final_word_across_encoder_alignment() -> anyhow::Result<()> {
    let manifest = PathBuf::from(std::env::var_os("CS_TRANSLATOR_TEST_ASR").expect("ASR manifest"));
    let path = PathBuf::from(
        std::env::var_os("CS_TRANSLATOR_TEST_ANNOUNCER_AUDIO").expect("announcer recording"),
    );
    let (audio, rate) = read_wav(&path)?;
    let mut asr = SpeechRecognizer::load(&manifest, 2, "auto", false)?;
    for prefix_ms in [0, 193, 333] {
        let mut waveform = vec![0.0; rate as usize * prefix_ms / 1000];
        waveform.extend_from_slice(&audio);
        let duration_ms = waveform.len() as u64 * 1000 / u64::from(rate);
        let length = rate as usize / 100;
        let mut finals = Vec::new();
        for (index, samples) in waveform.chunks(length).enumerate() {
            finals.extend(
                asr.push(&AudioChunk {
                    samples: samples.to_vec(),
                    sample_rate: rate,
                    timestamp_ms: (index * length) as u64 * 1000 / u64::from(rate),
                    discontinuity: false,
                })?
                .into_iter()
                .filter(|result| result.is_final),
            );
        }
        finals.extend(asr.finish()?.into_iter().filter(|result| result.is_final));
        let text = finals
            .iter()
            .map(|result| result.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            canonical(&text),
            "terroristswin",
            "EOF alignment: {prefix_ms} ms"
        );
        assert!(
            finals
                .iter()
                .all(|result| { result.start_ms < result.end_ms && result.end_ms <= duration_ms })
        );
    }
    Ok(())
}

#[test]
#[ignore = "Requires CS_TRANSLATOR_TEST_BOUNDARY_ASR (1-second Nemotron limit) and CS_TRANSLATOR_TEST_AUDIO (subsecond 48 kHz 'Behind you')"]
fn phrase_limit_does_not_discard_buffered_callout_tail() -> anyhow::Result<()> {
    let manifest = PathBuf::from(
        std::env::var_os("CS_TRANSLATOR_TEST_BOUNDARY_ASR").expect("1-second ASR manifest"),
    );
    let audio_path =
        PathBuf::from(std::env::var_os("CS_TRANSLATOR_TEST_AUDIO").expect("48 kHz fixture"));
    let (mut audio, rate) = read_wav(&audio_path)?;
    assert_eq!(rate, 48000);
    assert!(
        audio.len() < rate as usize,
        "Use the subsecond callout fixture"
    );
    let mut prefixed = vec![0.0; rate as usize * 8 / 10];
    prefixed.append(&mut audio);
    prefixed.resize(prefixed.len() + rate as usize * 6 / 10, 0.0);
    audio = prefixed;
    let mut asr = SpeechRecognizer::load(&manifest, 2, "auto", false)?;
    let mut finals = Vec::new();
    let length = rate as usize / 100;
    for (index, chunk) in audio.chunks(length).enumerate() {
        finals.extend(
            asr.push(&AudioChunk {
                samples: chunk.to_vec(),
                sample_rate: rate,
                timestamp_ms: (index * length) as u64 * 1000 / u64::from(rate),
                discontinuity: false,
            })?
            .into_iter()
            .filter(|r| r.is_final)
            .map(|r| r.text),
        );
    }
    finals.extend(
        asr.finish()?
            .into_iter()
            .filter(|r| r.is_final)
            .map(|r| r.text),
    );
    let words = finals
        .iter()
        .map(|text| canonical(text))
        .collect::<Vec<_>>();
    assert_eq!(
        words,
        ["behindyou"],
        "The phrase limit must not finalize before the encoder has decoded the last word: {finals:?}"
    );
    Ok(())
}

#[test]
#[ignore = "Requires CS_TRANSLATOR_TEST_ASR and CS_TRANSLATOR_TEST_AUDIO (48 kHz 'Behind you')"]
fn native_silence_setting_changes_endpoint_without_absorbing_leading_idle() -> anyhow::Result<()> {
    use std::io::Write;
    let manifest = PathBuf::from(std::env::var_os("CS_TRANSLATOR_TEST_ASR").expect("ASR manifest"));
    let audio_path =
        PathBuf::from(std::env::var_os("CS_TRANSLATOR_TEST_AUDIO").expect("48 kHz fixture"));
    let (audio, rate) = read_wav(&audio_path)?;
    assert_eq!(rate, 48000);
    let mut template: toml::Value = toml::from_str(&std::fs::read_to_string(&manifest)?)?;
    for (_, path) in template["files"]
        .as_table_mut()
        .expect("model files")
        .iter_mut()
    {
        let resolved = manifest
            .parent()
            .expect("manifest parent")
            .join(path.as_str().expect("path"));
        *path = toml::Value::String(resolved.to_str().expect("UTF-8 model path").to_owned());
    }
    let mut waveform = vec![0.0; rate as usize * 2];
    waveform.extend_from_slice(&audio);
    waveform.resize(waveform.len() + rate as usize * 4, 0.0);
    let mut endpoints = Vec::new();
    for silence in [0.5, 1.5] {
        template["silence_seconds"] = toml::Value::Float(silence);
        let mut configured = tempfile::NamedTempFile::new()?;
        configured.write_all(toml::to_string(&template)?.as_bytes())?;
        configured.flush()?;
        let mut asr = SpeechRecognizer::load(configured.path(), 2, "auto", false)?;
        let mut finals = Vec::new();
        let length = rate as usize / 100;
        for (index, samples) in waveform.chunks(length).enumerate() {
            for result in asr.push(&AudioChunk {
                samples: samples.to_vec(),
                sample_rate: rate,
                timestamp_ms: (index * length) as u64 * 1000 / u64::from(rate),
                discontinuity: false,
            })? {
                if result.is_final {
                    endpoints.push((index + 1) as u64 * 10);
                    finals.push(result);
                }
            }
        }
        assert_eq!(
            finals.len(),
            1,
            "Silence must finalize before finish: {finals:?}"
        );
        assert_eq!(canonical(&finals[0].text), "behindyou");
        assert!(
            finals[0].start_ms >= 1900,
            "Leading idle must not attract HUD labels: {finals:?}"
        );
        assert!(finals[0].end_ms >= finals[0].start_ms);
        assert!(asr.finish()?.into_iter().all(|r| r.text.trim().is_empty()));
    }
    assert!(
        endpoints[1].saturating_sub(endpoints[0]) >= 800,
        "A longer configured silence must delay native finalization, independent of encoder lookahead: {endpoints:?}"
    );
    Ok(())
}

#[test]
#[ignore = "Requires CS_TRANSLATOR_TEST_OFFLINE_ASR (offline model manifest)"]
fn empty_markers_do_not_select_offline_input_rate() -> anyhow::Result<()> {
    let manifest = PathBuf::from(
        std::env::var_os("CS_TRANSLATOR_TEST_OFFLINE_ASR").expect("offline ASR manifest"),
    );
    let mut asr = SpeechRecognizer::load(&manifest, 2, "auto", false)?;
    assert!(
        asr.push(&AudioChunk {
            samples: Vec::new(),
            sample_rate: 48000,
            timestamp_ms: 0,
            discontinuity: false
        })?
        .is_empty()
    );
    assert!(
        asr.push(&AudioChunk {
            samples: vec![0.0; 1600],
            sample_rate: 16000,
            timestamp_ms: 0,
            discontinuity: false
        })?
        .is_empty()
    );
    assert!(asr.finish()?.is_empty());
    Ok(())
}
