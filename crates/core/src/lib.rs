use std::{
    borrow::Cow,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::AtomicU64,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub timestamp_ms: u64,
    /// Starts a new audio timeline after a device gap or capture overrun.
    pub discontinuity: bool,
}

/// Out-of-band counters remain observable even while the bounded media queue is full.
#[derive(Debug, Default)]
pub struct CaptureDiagnostics {
    pub audio_packets_dropped: AtomicU64,
    pub hud_observations_dropped: AtomicU64,
    /// Pending HUD frames replaced by newer captures before OCR; not lost audio.
    pub hud_frames_coalesced: AtomicU64,
    pub chat_messages_dropped: AtomicU64,
    pub status_messages_dropped: AtomicU64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpeakerLabel {
    pub text: String,
    pub truncated: bool,
}

impl SpeakerLabel {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let text = text.trim().to_owned();
        let truncated = text.ends_with('…') || text.ends_with("...");
        Self { text, truncated }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlayerLanguage {
    pub player: SpeakerLabel,
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HudObservation {
    pub timestamp_ms: u64,
    pub players: Vec<SpeakerLabel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub timestamp_ms: u64,
    pub player: String,
    pub channel: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum CaptureEvent {
    Audio(AudioChunk),
    Chat(ChatMessage),
    Hud(HudObservation),
    Status {
        component: String,
        message: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recognition {
    pub text: String,
    pub language: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub is_final: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caption {
    pub id: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub channel: String,
    pub speakers: Vec<SpeakerLabel>,
    pub original: String,
    pub translated: String,
    pub translation_pending: bool,
    #[serde(default)]
    pub translation_error: Option<String>,
    pub language: Option<String>,
    pub is_final: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppEvent {
    Caption(Caption),
    Ready,
    Status {
        component: String,
        message: String,
        is_error: bool,
    },
}

/// Limits revised-partial translation work without delaying the first or final result.
pub struct TranslationCadence {
    interval_ms: u64,
    last_started_ms: Option<u64>,
}

impl TranslationCadence {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            last_started_ms: None,
        }
    }

    pub fn due(&self, now_ms: u64, is_final: bool) -> bool {
        is_final
            || self
                .last_started_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= self.interval_ms)
    }

    pub fn started(&mut self, now_ms: u64) {
        self.last_started_ms = Some(now_ms);
    }

    pub fn reset(&mut self) {
        self.last_started_ms = None;
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NormalizedRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl NormalizedRect {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [self.x, self.y, self.width, self.height]
                .iter()
                .all(|v| v.is_finite()),
            "HUD rectangle must be finite"
        );
        ensure!(
            self.x >= 0.0
                && self.y >= 0.0
                && self.width > 0.0
                && self.height > 0.0
                && self.x + self.width <= 1.0001
                && self.y + self.height <= 1.0001,
            "HUD rectangle must lie within the captured image (normalized 0..1)"
        );
        Ok(())
    }

    pub fn pixels(&self, width: u32, height: u32) -> Result<(u32, u32, u32, u32)> {
        self.validate()?;
        ensure!(width > 0 && height > 0, "Captured image is empty");
        let x = ((self.x * width as f32).floor() as u32).min(width - 1);
        let y = ((self.y * height as f32).floor() as u32).min(height - 1);
        let w = ((self.width * width as f32).ceil() as u32)
            .max(1)
            .min(width - x);
        let h = ((self.height * height as f32).ceil() as u32)
            .max(1)
            .min(height - y);
        Ok((x, y, w, h))
    }
}

impl Default for NormalizedRect {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.76,
            width: 0.28,
            height: 0.17,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub setup_completed: bool,
    pub target_language: String,
    pub source_language: String,
    pub voice_enabled: bool,
    pub speech_beam_search: bool,
    pub threads: usize,
    pub translation_beam_size: usize,
    pub partial_translation_interval_ms: u64,
    pub background_inference: bool,
    pub asr_manifest: PathBuf,
    pub translation_model: PathBuf,
    pub ocr_config: PathBuf,
    pub console_log: Option<PathBuf>,
    pub process_id: Option<u32>,
    pub capture_window: Option<isize>,
    pub hud_enabled: bool,
    pub overlay_enabled: bool,
    pub show_original: bool,
    pub show_message_labels: bool,
    pub caption_font_size: u8,
    pub overlay_show_original: bool,
    pub overlay_font_size: u8,
    pub overlay_opacity: u8,
    pub overlay_message_expiry_secs: u16,
    pub overlay_x: f32,
    pub overlay_y: f32,
    pub overlay_width: f32,
    pub overlay_height: f32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            setup_completed: false,
            target_language: "en".into(),
            source_language: "auto".into(),
            voice_enabled: true,
            speech_beam_search: false,
            threads: 2,
            translation_beam_size: 1,
            partial_translation_interval_ms: 1000,
            background_inference: true,
            asr_manifest: "model-manifests/nemotron-multilingual.toml".into(),
            translation_model: "models/m2m100-int8".into(),
            ocr_config: "models/ocr/ocr.toml".into(),
            console_log: None,
            process_id: None,
            capture_window: None,
            hud_enabled: false,
            overlay_enabled: true,
            show_original: true,
            show_message_labels: true,
            caption_font_size: 15,
            overlay_show_original: false,
            overlay_font_size: 13,
            overlay_opacity: 50,
            overlay_message_expiry_secs: 8,
            overlay_x: 400.0,
            overlay_y: 780.0,
            overlay_width: 560.0,
            overlay_height: 150.0,
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("Read settings {}", path.display()))?;
        let mut config: Self = toml::from_str(&text).context("Parse settings TOML")?;
        config.validate()?;
        config.normalize_languages()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let mut config = self.clone();
        config.normalize_languages()?;
        let text = toml::to_string_pretty(&config)?;
        let temporary = stage_settings(path, text.as_bytes())?;
        temporary
            .persist(path)
            .with_context(|| format!("Replace settings {}", path.display()))?;
        Ok(())
    }

    fn normalize_languages(&mut self) -> Result<()> {
        self.target_language = normalize_language(&self.target_language)?;
        self.source_language = if self.source_language.trim().eq_ignore_ascii_case("auto") {
            "auto".into()
        } else {
            normalize_language(&self.source_language)?
        };
        Ok(())
    }

    pub fn translation_source<'a>(
        &'a self,
        player_languages: &'a [PlayerLanguage],
        speakers: &[SpeakerLabel],
        detected: Option<&'a str>,
    ) -> &'a str {
        if let [speaker] = speakers
            && let Some(tag) = player_languages.iter().find(|tag| &tag.player == speaker)
        {
            return &tag.language;
        }
        if !self.source_language.eq_ignore_ascii_case("auto") {
            return &self.source_language;
        }
        detected.unwrap_or("auto")
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=16).contains(&self.threads),
            "Inference threads must be between 1 and 16"
        );
        ensure!(
            (1..=2).contains(&self.translation_beam_size),
            "Translation beam size must be 1 or 2"
        );
        ensure!(
            self.partial_translation_interval_ms <= 5000,
            "Partial translation interval must be between 0 and 5000 ms"
        );
        ensure!(
            (12..=24).contains(&self.caption_font_size),
            "Caption font size must be between 12 and 24 px"
        );
        ensure!(
            (10..=22).contains(&self.overlay_font_size),
            "Overlay font size must be between 10 and 22 px"
        );
        ensure!(
            (10..=90).contains(&self.overlay_opacity),
            "Overlay opacity must be between 10 and 90 percent"
        );
        ensure!(
            (1..=60).contains(&self.overlay_message_expiry_secs),
            "Overlay message expiry must be between 1 and 60 seconds"
        );
        normalize_language(&self.target_language).context("Invalid target language")?;
        if !self.source_language.trim().eq_ignore_ascii_case("auto") {
            normalize_language(&self.source_language).context("Invalid source language")?;
        }
        ensure!(
            [
                self.overlay_x,
                self.overlay_y,
                self.overlay_width,
                self.overlay_height
            ]
            .iter()
            .all(|v| v.is_finite())
                && self.overlay_width > 0.0
                && self.overlay_height > 0.0,
            "Overlay dimensions must be finite and positive"
        );
        Ok(())
    }
}

/// Normalize a locale to its ASCII two- or three-letter primary language code.
/// Locale subtags must be nonempty ASCII alphanumeric segments.
pub fn normalize_language(language: &str) -> Result<String> {
    let mut parts = language.trim().split(['-', '_']);
    let primary = parts.next().unwrap_or_default();
    ensure!(
        (2..=3).contains(&primary.len()) && primary.bytes().all(|b| b.is_ascii_alphabetic()),
        "Language must use a two- or three-letter ASCII code: {language:?}"
    );
    ensure!(
        parts.all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric())),
        "Invalid locale subtags: {language:?}"
    );
    if primary.eq_ignore_ascii_case("iw") {
        Ok("he".into())
    } else if primary.eq_ignore_ascii_case("in") {
        Ok("id".into())
    } else {
        Ok(primary.to_ascii_lowercase())
    }
}

fn stage_settings(path: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("Stage settings {}", path.display()))?;
    temporary
        .write_all(bytes)
        .context("Write staged settings")?;
    temporary
        .as_file()
        .sync_all()
        .context("Sync staged settings")?;
    Ok(temporary)
}

pub fn settings_path() -> PathBuf {
    std::env::var_os("CS_TRANSLATOR_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("CS Translator"))
        })
        .unwrap_or_else(|| PathBuf::from("local"))
        .join("settings.toml")
}

pub fn parse_chat_line(line: &str, timestamp_ms: u64) -> Option<ChatMessage> {
    let clean = sanitize_chat_line(line);
    let clean = clean.trim();
    let start = clean.find('[')?;
    if !clean[..start]
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_ascii_whitespace() || "/:.-*".contains(c))
    {
        return None;
    }
    let rest = &clean[start..];
    let rest = rest
        .strip_prefix("[Client]")
        .map(str::trim_start)
        .unwrap_or(rest);
    let rest = rest.strip_prefix('[')?;
    let end = rest.find(']')?;
    let channel = &rest[..end];
    if !matches!(channel, "ALL" | "T" | "CT" | "SPEC" | "TEAM") {
        return None;
    }
    let (player, text) = rest[end + 1..].trim_start().split_once(": ")?;
    if player.trim().is_empty() || text.trim().is_empty() {
        return None;
    }
    Some(ChatMessage {
        timestamp_ms,
        player: player.trim().to_owned(),
        channel: channel.to_owned(),
        text: text.trim().to_owned(),
    })
}

fn sanitize_chat_line(line: &str) -> Cow<'_, str> {
    if line.chars().any(|c| c.is_control() && c != '\t') {
        Cow::Owned(
            line.chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect(),
        )
    } else {
        Cow::Borrowed(line)
    }
}

/// Returns observed candidate labels, not a forced identity for overlapped speech.
pub fn speakers_for_interval(
    observations: &[HudObservation],
    start_ms: u64,
    end_ms: u64,
) -> Vec<SpeakerLabel> {
    let mut players = Vec::new();
    for observation in observations.iter().filter(|o| {
        o.timestamp_ms.saturating_add(180) >= start_ms
            && o.timestamp_ms <= end_ms.saturating_add(100)
    }) {
        for player in &observation.players {
            if !player.text.is_empty() && !players.contains(player) {
                players.push(player.clone());
            }
        }
    }
    players
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obsolete_speaker_geometry_is_discarded_without_affecting_enablement() {
        let config: AppConfig = toml::from_str(
            "hud_enabled = true\n[speaker_calibration]\nversion = 1\nframe_width = 1920\nframe_height = 1080\nicon = 'obsolete.png'\n",
        )
        .unwrap();
        assert!(config.hud_enabled);
        let saved = toml::to_string(&config).unwrap();
        assert!(!saved.contains("speaker_calibration"));
        assert!(!saved.contains("obsolete.png"));
    }

    #[test]
    fn settings_reject_invalid_languages_and_normalize_locales() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("settings.toml");
        for text in [
            "source_language = 'english'",
            "target_language = 'english'",
            "target_language = 'auto'",
            "source_language = 'en--US'",
            "target_language = 'ja_猫'",
        ] {
            fs::write(&path, text).expect("write invalid settings");
            assert!(AppConfig::load(&path).is_err(), "{text}");
        }
        fs::write(
            &path,
            "source_language = ' JA_jp '\ntarget_language = ' ru-RU '",
        )
        .expect("write locales");
        let config = AppConfig::load(&path).expect("load locales");
        assert_eq!(config.source_language, "ja");
        assert_eq!(config.target_language, "ru");
        assert_eq!(
            normalize_language(" FIL_ph ").expect("three-letter locale"),
            "fil"
        );
        assert_eq!(normalize_language("IW_il").expect("legacy Hebrew"), "he");
        assert_eq!(
            normalize_language("in-ID").expect("legacy Indonesian"),
            "id"
        );
        fs::write(&path, "source_language = ' AUTO '").expect("write auto");
        assert_eq!(
            AppConfig::load(&path).expect("load auto").source_language,
            "auto"
        );
    }

    #[test]
    fn settings_staging_and_failed_replacement_preserve_previous_preferences() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("settings.toml");
        let previous = AppConfig {
            target_language: "ja".into(),
            ..Default::default()
        };
        previous.save(&path).expect("save previous preferences");
        let old_bytes = fs::read(&path).expect("read previous preferences");
        let replacement = toml::to_string(&AppConfig::default()).expect("serialize replacement");

        // An interrupted writer never publishes its staged bytes.
        let staged = stage_settings(&path, replacement.as_bytes()).expect("stage replacement");
        drop(staged);
        assert_eq!(fs::read(&path).expect("read after interruption"), old_bytes);

        // Lose the staged source before the OS replacement operation.
        let staged = stage_settings(&path, replacement.as_bytes()).expect("stage replacement");
        fs::remove_file(staged.path()).expect("remove staged source");
        assert!(staged.persist(&path).is_err());
        assert_eq!(fs::read(&path).expect("read after failure"), old_bytes);
        assert_eq!(
            AppConfig::load(&path)
                .expect("load old settings")
                .target_language,
            "ja"
        );

        AppConfig {
            target_language: " RU_ru ".into(),
            ..Default::default()
        }
        .save(&path)
        .expect("replace preferences");
        assert_eq!(
            AppConfig::load(&path)
                .expect("load replacement")
                .target_language,
            "ru"
        );
    }

    #[test]
    fn chat_sanitation_preserves_tabs_and_unicode_without_copying_clean_lines() {
        let ordinary = "network status: connected";
        assert!(matches!(sanitize_chat_line(ordinary), Cow::Borrowed(_)));
        assert!(parse_chat_line(ordinary, 0).is_none());
        let message = parse_chat_line("\u{1}[CT] 猫\u{2}: идём\tB\r\n", 12)
            .expect("chat with embedded controls");
        assert_eq!(message.player, "猫");
        assert_eq!(message.text, "идём\tB");
        assert_eq!(message.timestamp_ms, 12);
    }

    #[test]
    fn overlay_message_expiry_enforces_usable_duration_bounds() {
        for (seconds, valid) in [(0, false), (1, true), (60, true), (61, false)] {
            let config = AppConfig {
                overlay_message_expiry_secs: seconds,
                ..Default::default()
            };
            assert_eq!(config.validate().is_ok(), valid, "expiry {seconds}");
        }
    }

    #[test]
    fn appearance_settings_enforce_readable_size_and_opacity_bounds() {
        for (caption_font_size, overlay_font_size, overlay_opacity) in [(12, 10, 10), (24, 22, 90)]
        {
            assert!(
                AppConfig {
                    caption_font_size,
                    overlay_font_size,
                    overlay_opacity,
                    ..Default::default()
                }
                .validate()
                .is_ok()
            );
        }
        for config in [
            AppConfig {
                caption_font_size: 11,
                ..Default::default()
            },
            AppConfig {
                caption_font_size: 25,
                ..Default::default()
            },
            AppConfig {
                overlay_font_size: 9,
                ..Default::default()
            },
            AppConfig {
                overlay_font_size: 23,
                ..Default::default()
            },
            AppConfig {
                overlay_opacity: 9,
                ..Default::default()
            },
            AppConfig {
                overlay_opacity: 91,
                ..Default::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn partial_translation_cadence_preserves_first_final_and_boundary_updates() {
        let mut cadence = TranslationCadence::new(1000);
        assert!(cadence.due(50, false));
        cadence.started(50);
        assert!(!cadence.due(1049, false));
        assert!(cadence.due(1050, false));
        assert!(cadence.due(51, true));
        cadence.reset();
        assert!(cadence.due(52, false));
    }

    #[test]
    fn immediate_translation_cadence_does_not_coalesce_updates() {
        let mut cadence = TranslationCadence::new(0);
        cadence.started(200);
        assert!(cadence.due(200, false));
    }

    #[test]
    fn chat_preserves_unicode_and_message_colons() {
        let message = parse_chat_line("[CT] Игрок猫: идём B: быстро", 41).expect("chat");
        assert_eq!(message.player, "Игрок猫");
        assert_eq!(message.text, "идём B: быстро");
        assert!(parse_chat_line("network [CT] noise: not chat", 41).is_none());
        let modern = parse_chat_line("11/06 00:12:25 [Client] [CT] Игрок猫: идём B: быстро", 42)
            .expect("timestamped client chat");
        assert_eq!(modern.player, "Игрок猫");
        assert_eq!(modern.text, "идём B: быстро");
        assert!(parse_chat_line("11/06 00:12:25 [Server] [CT] noise: not chat", 42).is_none());
    }

    #[test]
    fn attribution_keeps_stacked_truncated_labels_and_rejects_stale_rows() {
        let observations = vec![
            HudObservation {
                timestamp_ms: 100,
                players: vec![SpeakerLabel::new("old")],
            },
            HudObservation {
                timestamp_ms: 1000,
                players: vec![SpeakerLabel::new("Игрок…"), SpeakerLabel::new("猫★")],
            },
        ];
        let labels = speakers_for_interval(&observations, 990, 1100);
        assert_eq!(
            labels,
            vec![SpeakerLabel::new("Игрок…"), SpeakerLabel::new("猫★")]
        );
        assert!(labels[0].truncated);
    }

    #[test]
    fn player_source_precedence_preserves_exact_observed_labels() {
        let player = SpeakerLabel::new("猫★Игрок…");
        let mut config = AppConfig {
            source_language: "de".into(),
            ..Default::default()
        };
        let tags = vec![PlayerLanguage {
            player: player.clone(),
            language: "ja".into(),
        }];
        assert_eq!(
            config.translation_source(&tags, std::slice::from_ref(&player), Some("ru")),
            "ja"
        );
        assert_eq!(
            config.translation_source(
                &tags,
                &[player.clone(), SpeakerLabel::new("other")],
                Some("ru")
            ),
            "de"
        );
        assert_eq!(
            config.translation_source(
                &tags,
                &[SpeakerLabel {
                    text: player.text.clone(),
                    truncated: false
                }],
                Some("ru")
            ),
            "de"
        );
        assert_eq!(
            config.translation_source(&tags, &[SpeakerLabel::new("猫★игрок…")], Some("ru")),
            "de"
        );
        assert_eq!(
            config.translation_source(&tags, &[SpeakerLabel::new("猫★Игрок")], Some("ru")),
            "de"
        );
        config.source_language = "auto".into();
        assert_eq!(config.translation_source(&tags, &[], Some("ru")), "ru");
        assert_eq!(config.translation_source(&tags, &[], None), "auto");
        assert_eq!(
            config.translation_source(&tags, &[player, SpeakerLabel::new("other")], Some("ru")),
            "ru"
        );
    }

    #[test]
    fn settings_do_not_retain_old_player_language_choices() {
        let config: AppConfig = toml::from_str(
            "source_language = 'de'\n[[player_languages]]\nplayer = { text = 'old', truncated = false }\nlanguage = 'ja'\n",
        ).unwrap();
        let saved = toml::to_string(&config).unwrap();
        assert!(!saved.contains("player_languages"));
        assert_eq!(
            config.translation_source(&[], &[SpeakerLabel::new("old")], Some("ru")),
            "de"
        );
    }

    #[test]
    fn crop_is_bounded_and_invalid_rectangles_fail() {
        assert_eq!(
            NormalizedRect {
                x: 0.9,
                y: 0.9,
                width: 0.1,
                height: 0.1
            }
            .pixels(101, 101)
            .expect("crop"),
            (90, 90, 11, 11)
        );
        assert!(
            NormalizedRect {
                x: f32::NAN,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            NormalizedRect {
                x: 0.9,
                width: 0.2,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
