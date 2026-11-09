use anyhow::{Context, Result, ensure};
use cs_core::{HudObservation, SpeakerLabel};
use image::{RgbaImage, imageops::FilterType};
use oar_ocr_core::{
    core::config::OrtSessionConfig,
    models::recognition::crnn::{CRNNModel, CRNNModelBuilder},
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecognizerConfig {
    model: PathBuf,
    dictionary: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OcrConfig {
    recognizers: Vec<RecognizerConfig>,
    runtime: PathBuf,
    #[serde(default)]
    speaker_icon: PathBuf,
    #[serde(default = "icon_threshold")]
    icon_threshold: f32,
    #[serde(default = "icon_scale")]
    icon_scale: f32,
    #[serde(default = "name_offset")]
    name_offset_icon_heights: f32,
    /// Zero keeps the visible field for custom templates; nonzero is a hard edge.
    #[serde(default)]
    name_width_icon_heights: f32,
    #[serde(default = "location_suffix")]
    strip_location_field: bool,
    #[serde(default)]
    icon_band_left: u32,
    #[serde(default = "icon_band_right")]
    icon_band_right: u32,
    #[serde(default = "threads")]
    threads: usize,
    #[serde(default = "confidence")]
    minimum_confidence: f32,
    #[serde(default = "interval")]
    capture_interval_ms: u64,
    #[serde(default = "scale")]
    upscale: u32,
}
fn threads() -> usize {
    2
}
fn confidence() -> f32 {
    0.55
}
fn interval() -> u64 {
    125
}
fn scale() -> u32 {
    2
}
fn icon_threshold() -> f32 {
    0.82
}
fn icon_scale() -> f32 {
    1.0
}
fn name_offset() -> f32 {
    3.6
}
fn location_suffix() -> bool {
    true
}
fn icon_band_right() -> u32 {
    64
}

pub struct HudOcr {
    pipelines: Vec<CRNNModel>,
    token_expansions: Vec<TokenExpansions>,
    pool: rayon::ThreadPool,
    matcher: MatcherWorkspace,
    minimum_confidence: f32,
    upscale: u32,
    capture_interval_ms: u64,
    previous: Vec<u8>,
    previous_dimensions: (u32, u32),
    previous_players: Vec<SpeakerLabel>,
    icon_templates: Vec<IconTemplate>,
    icon_threshold: f32,
    name_offset_icon_heights: f32,
    name_width_icon_heights: f32,
    strip_location_field: bool,
    icon_band: (u32, u32),
    previous_fields: Vec<image::RgbImage>,
    search: SearchState,
    layout_hint: Option<crate::HudLayoutHint>,
    standard_icon: bool,
    region: cs_core::NormalizedRect,
}

impl HudOcr {
    pub fn capture_interval_ms(&self) -> u64 {
        self.capture_interval_ms
    }

    pub fn region(&self) -> cs_core::NormalizedRect {
        self.region
    }

    /// Saved settings only prioritize a search; a real glyph is always required.
    pub fn set_layout_hint(&mut self, hint: Option<crate::HudLayoutHint>) {
        self.layout_hint = hint;
        self.invalidate_layout();
    }

    pub fn invalidate_layout(&mut self) {
        self.previous.clear();
        self.previous_fields.clear();
        self.previous_players.clear();
        self.search = SearchState::default();
    }

    pub fn load(config_path: &Path) -> Result<Self> {
        let config: OcrConfig =
            toml::from_str(&crate::read_config(config_path)?).context("Parse OCR configuration")?;
        ensure!(
            (1..=16).contains(&config.threads),
            "OCR threads must be 1..16"
        );
        ensure!(
            (1..=4).contains(&config.upscale),
            "OCR upscale must be 1..4"
        );
        ensure!(
            (40..=5000).contains(&config.capture_interval_ms),
            "OCR interval must be 40..5000 ms"
        );
        ensure!(
            config.minimum_confidence.is_finite()
                && (0.0..=1.0).contains(&config.minimum_confidence),
            "Invalid OCR confidence"
        );
        ensure!(
            !config.recognizers.is_empty() && config.recognizers.len() <= 8,
            "Configure 1..8 OCR script recognizers"
        );
        let root = config_path.parent().unwrap_or_else(|| Path::new("."));
        ensure!(
            config.icon_threshold.is_finite() && (0.5..=1.0).contains(&config.icon_threshold),
            "Icon threshold must be 0.5..1"
        );
        ensure!(
            config.icon_scale.is_finite() && (0.25..=4.0).contains(&config.icon_scale),
            "Icon scale must be 0.25..4"
        );
        ensure!(
            config.name_offset_icon_heights.is_finite()
                && (1.0..=10.0).contains(&config.name_offset_icon_heights),
            "Name offset must be 1..10 icon heights"
        );
        ensure!(
            config.name_width_icon_heights.is_finite()
                && (0.0..=100.0).contains(&config.name_width_icon_heights),
            "Name width must be 0..100 icon heights"
        );
        ensure!(
            config.icon_band_left < config.icon_band_right && config.icon_band_right <= 8192,
            "Icon band must be an increasing pixel interval within the cropped ROI"
        );
        let standard_icon = config.speaker_icon.as_os_str().is_empty();
        let icon_templates = if standard_icon {
            standard_templates()
        } else {
            let icon_path = root.join(&config.speaker_icon);
            let icon = image::open(&icon_path)
                .with_context(|| format!("Load custom speaker icon {}", icon_path.display()))?
                .to_luma8();
            ensure!(
                (5..=160).contains(&icon.width()) && (5..=160).contains(&icon.height()),
                "Custom speaker template must be a tight 5..160-pixel crop"
            );
            let minimum = icon.as_raw().iter().copied().min().unwrap_or(0);
            let maximum = icon.as_raw().iter().copied().max().unwrap_or(0);
            ensure!(
                maximum.saturating_sub(minimum) >= 12,
                "Speaker template has no usable contrast"
            );
            [0.95_f32, 1.0, 1.05]
                .into_iter()
                .map(|scale| {
                    let scale = scale * config.icon_scale;
                    IconTemplate::new(image::imageops::resize(
                        &icon,
                        (icon.width() as f32 * scale).round() as u32,
                        (icon.height() as f32 * scale).round() as u32,
                        FilterType::Triangle,
                    ))
                })
                .collect()
        };
        initialize_runtime(&root.join(&config.runtime))?;
        let mut pipelines = Vec::new();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.threads)
            .thread_name(|index| format!("hud-ocr-{index}"))
            .build()
            .context("Create bounded OCR preprocessing pool")?;
        let mut token_expansions = Vec::new();
        for recognizer in config.recognizers {
            let model = root.join(recognizer.model);
            let dictionary = root.join(recognizer.dictionary);
            ensure!(
                model.is_file() && dictionary.is_file(),
                "Missing OCR recognizer {} or its matching dictionary {}",
                model.display(),
                dictionary.display()
            );
            let session = OrtSessionConfig::new()
                .with_intra_threads(config.threads)
                .with_inter_threads(1)
                .with_parallel_execution(false);
            let dictionary = std::fs::read_to_string(&dictionary)
                .with_context(|| format!("Read OCR dictionary {}", dictionary.display()))?;
            let (characters, expansions) = encode_dictionary(&dictionary)?;
            token_expansions.push(expansions);
            pipelines.push(
                CRNNModelBuilder::new()
                    .character_dict(characters)
                    .with_ort_config(session)
                    .build(model)
                    .context("Load native local OCR row recognizer")?,
            );
        }
        Ok(Self {
            pipelines,
            token_expansions,
            pool,
            matcher: MatcherWorkspace::default(),
            minimum_confidence: config.minimum_confidence,
            upscale: config.upscale,
            capture_interval_ms: config.capture_interval_ms,
            previous: Vec::new(),
            previous_dimensions: (0, 0),
            previous_players: Vec::new(),
            icon_templates,
            icon_threshold: if standard_icon {
                config.icon_threshold.max(0.91)
            } else {
                config.icon_threshold
            },
            name_offset_icon_heights: if standard_icon {
                4.4
            } else {
                config.name_offset_icon_heights
            },
            // Whole-area rows include unrelated world pixels and compress short
            // HUD names beyond useful scale. Keep hard-crop uncertainty visible.
            name_width_icon_heights: if standard_icon && config.name_width_icon_heights == 0.0 {
                14.0
            } else {
                config.name_width_icon_heights
            },
            strip_location_field: config.strip_location_field,
            previous_fields: Vec::new(),
            icon_band: (config.icon_band_left, config.icon_band_right),
            search: SearchState::default(),
            layout_hint: None,
            standard_icon,
            region: crate::standard_speaker_region(),
        })
    }

    pub fn recognize(&mut self, image: &RgbaImage, timestamp_ms: u64) -> Result<HudObservation> {
        if self.previous_dimensions != image.dimensions() {
            self.invalidate_layout();
        }
        ensure!(
            image.width() > 0 && image.height() > 0,
            "Cannot OCR an empty HUD frame"
        );
        ensure!(
            image.width() <= 8192 && image.height() <= 8192,
            "HUD frame exceeds 8192 pixels per side"
        );
        let icon_band = if self.standard_icon {
            (0, image.width())
        } else {
            self.icon_band
        };
        validate_frame_band(image, icon_band)?;
        // A static frame can outlive the broad-search backoff after its voice
        // column moves. An empty cached result must not suppress reacquisition.
        let reacquire_due = self.standard_icon
            && self.previous_players.is_empty()
            && self
                .search
                .last_broad
                .is_none_or(|last| timestamp_ms.saturating_sub(last) >= 2000);
        if !reacquire_due
            && self.previous_dimensions == image.dimensions()
            && self.previous == image.as_raw()[..]
        {
            return Ok(HudObservation {
                timestamp_ms,
                players: self.previous_players.clone(),
            });
        }
        let mut icons = if self.standard_icon {
            self.search.find(
                image,
                &self.icon_templates,
                self.icon_threshold,
                self.layout_hint,
                timestamp_ms,
                &mut self.matcher,
            )
        } else {
            find_icons(
                image,
                &self.icon_templates,
                self.icon_threshold,
                icon_band,
                &mut self.matcher,
            )
        };
        icons.sort_by_key(|icon| icon.y);
        // A voice panel cannot contain an unbounded number of simultaneous rows.
        // Busy/unsupported layouts are not evidence for arbitrary text labels.
        if icons.len() > 8 {
            icons.clear();
        }
        if icons.is_empty() {
            self.previous_fields.clear();
            self.previous.clear();
            self.previous.extend_from_slice(image.as_raw());
            self.previous_dimensions = image.dimensions();
            self.previous_players.clear();
            return Ok(HudObservation {
                timestamp_ms,
                players: Vec::new(),
            });
        }
        let mut fields = Vec::with_capacity(icons.len());
        let mut clipped = Vec::with_capacity(icons.len());
        for icon in &icons {
            let (left, top, right, bottom, hard_clipped) = {
                let left =
                    (icon.x as f32 + icon.height as f32 * self.name_offset_icon_heights) as u32;
                let top = icon.y.saturating_sub(icon.height / 4);
                let bottom = (icon.y + icon.height + icon.height / 4).min(image.height());
                let (right, clipped) = name_right_edge(
                    left,
                    icon.height,
                    self.name_width_icon_heights,
                    image.width(),
                );
                (left, top, right, bottom, clipped)
            };
            if left >= right {
                continue;
            }
            let field = image::RgbImage::from_fn(right - left, bottom - top, |x, y| {
                let p = image.get_pixel(left + x, top + y).0;
                image::Rgb([p[0], p[1], p[2]])
            });
            fields.push(if self.upscale > 1 {
                image::imageops::resize(
                    &field,
                    field.width() * self.upscale,
                    field.height() * self.upscale,
                    FilterType::CatmullRom,
                )
            } else {
                field
            });
            clipped.push(hard_clipped);
        }
        // Compare only name fields: animated world pixels outside them do not rerun OCR.
        let players = if fields == self.previous_fields {
            self.previous_players.clone()
        } else {
            let references: Vec<_> = fields.iter().collect();
            let mut best: Vec<Option<(String, f32)>> = vec![None; fields.len()];
            if !references.is_empty() {
                for (pipeline, expansions) in self.pipelines.iter().zip(&self.token_expansions) {
                    let result = self
                        .pool
                        .install(|| pipeline.forward_refs(&references, false))
                        .context("Recognize confirmed speaker name rows")?;
                    for (index, (text, score)) in
                        result.texts.into_iter().zip(result.scores).enumerate()
                    {
                        if index >= best.len()
                            || !score.is_finite()
                            || score < self.minimum_confidence
                            || text.trim().is_empty()
                        {
                            continue;
                        }
                        if best[index].as_ref().is_none_or(|(_, old)| score > *old) {
                            let text = if expansions.is_empty() {
                                text
                            } else {
                                expand_tokens(&text, expansions)
                            };
                            best[index] = Some((text, score));
                        }
                    }
                }
            }
            best.into_iter()
                .enumerate()
                .filter_map(|(index, candidate)| {
                    let (text, _) = candidate?;
                    let mut label = name_field(&text, self.strip_location_field);
                    label.truncated |= clipped[index];
                    Some(label)
                })
                .collect()
        };
        self.previous_fields = fields;
        self.previous.clear();
        self.previous.extend_from_slice(image.as_raw());
        self.previous_dimensions = image.dimensions();
        self.previous_players.clone_from(&players);
        Ok(HudObservation {
            timestamp_ms,
            players,
        })
    }
}

fn name_field(text: &str, strip_location: bool) -> SpeakerLabel {
    let name = if strip_location {
        text.split_once(" @ ").map_or(text, |(name, _)| name)
    } else {
        text
    };
    SpeakerLabel::new(name.trim())
}

fn initialize_runtime(path: &Path) -> Result<()> {
    static RUNTIME: std::sync::Mutex<Option<(PathBuf, libloading::Library)>> =
        std::sync::Mutex::new(None);
    let canonical = path.canonicalize().with_context(|| format!("Missing private OCR ONNX Runtime {}: install the matching ONNX Runtime CPU DLL under this distinct filename", path.display()))?;
    let mut initialized = RUNTIME
        .lock()
        .map_err(|_| anyhow::anyhow!("OCR runtime initialization lock poisoned"))?;
    if let Some((loaded, _library)) = initialized.as_ref() {
        ensure!(
            *loaded == canonical,
            "OCR runtime already initialized from {}; cannot switch to {}",
            loaded.display(),
            canonical.display()
        );
        return Ok(());
    }
    // Own fallible loading: ort rc.13 marks its internal OnceLock complete even
    // when loading fails. Publishing an API only after validation permits retry.
    let library = unsafe { libloading::Library::new(&canonical) }
        .with_context(|| format!("Load private OCR ONNX Runtime {}", canonical.display()))?;
    let get_base: libloading::Symbol<unsafe extern "C" fn() -> *const ort::sys::OrtApiBase> =
        unsafe { library.get(b"OrtGetApiBase") }.context("OCR runtime lacks OrtGetApiBase")?;
    let base = unsafe { get_base() };
    ensure!(!base.is_null(), "OCR runtime returned a null API base");
    let api = unsafe { ((*base).GetApi)(ort::MINOR_VERSION) };
    ensure!(
        !api.is_null(),
        "OCR runtime does not support ONNX API {}; install its matching CPU runtime",
        ort::MINOR_VERSION
    );
    ensure!(
        ort::init().with_telemetry(false).commit(),
        "OCR ONNX Runtime was configured before its private telemetry-disabled environment; restart the app"
    );
    // ort's supported custom-loading API copies the function table; retain the
    // library for the process lifetime so every function pointer remains valid.
    ensure!(
        ort::set_api(unsafe { api.read() }),
        "OCR ONNX API was initialized outside its private runtime; restart the app"
    );
    *initialized = Some((canonical, library));
    Ok(())
}

fn validate_frame_band(image: &RgbaImage, band: (u32, u32)) -> Result<()> {
    ensure!(
        band.0 < band.1 && band.1 <= image.width(),
        "OCR icon band {}..{} lies outside {}x{} HUD ROI; adjust the diagnostic icon band",
        band.0,
        band.1,
        image.width(),
        image.height()
    );
    Ok(())
}

fn name_right_edge(
    left: u32,
    icon_height: u32,
    configured_width: f32,
    frame_width: u32,
) -> (u32, bool) {
    let hard_clipped = configured_width > 0.0;
    let right = if hard_clipped {
        left.saturating_add((icon_height as f32 * configured_width) as u32)
            .min(frame_width)
    } else {
        frame_width
    };
    (right, hard_clipped)
}

// OAR's decoder stores one scalar per vocabulary entry. Substitute collision-free
// private-use scalars while keeping every model vocabulary index unchanged.
type TokenExpansions = Vec<(char, String)>;
type EncodedDictionary = (Vec<String>, TokenExpansions);

fn encode_dictionary(dictionary: &str) -> Result<EncodedDictionary> {
    let occupied: std::collections::HashSet<char> = dictionary.chars().collect();
    let mut sentinels = (0xF0000..=0xFFFFD)
        .chain(0x100000..=0x10FFFD)
        .filter_map(char::from_u32)
        .filter(|c| !occupied.contains(c));
    let mut expansions = Vec::new();
    let mut tokens = Vec::new();
    for token in dictionary.lines() {
        if token.chars().count() > 1 {
            let sentinel = sentinels
                .next()
                .context("OCR dictionary exhausts private-use token scalars")?;
            expansions.push((sentinel, token.to_owned()));
            tokens.push(sentinel.to_string());
        } else {
            tokens.push(token.to_owned());
        }
    }
    Ok((tokens, expansions))
}

fn expand_tokens(text: &str, expansions: &[(char, String)]) -> String {
    let mut output = String::with_capacity(text.len());
    for scalar in text.chars() {
        if let Some((_, token)) = expansions.iter().find(|(sentinel, _)| *sentinel == scalar) {
            output.push_str(token);
        } else {
            output.push(scalar);
        }
    }
    output
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tracked_column_searches_stacked_rows_and_broadly_reacquires() {
        let templates = standard_templates();
        let mut workspace = MatcherWorkspace::default();
        let mut tracker = SearchState::default();
        let paint = |x, height, rows: &[u32]| {
            let mut frame = RgbaImage::from_pixel(240, 160, image::Rgba([20, 20, 20, 255]));
            let glyph = standard_stencil(height);
            for &top in rows {
                for y in 0..glyph.height() {
                    for dx in 0..glyph.width() {
                        let v = glyph.get_pixel(dx, y)[0];
                        frame.put_pixel(x + dx, top + y, image::Rgba([v, v, v, 255]));
                    }
                }
            }
            frame
        };
        let first = paint(20, 18, &[25, 70]);
        assert_eq!(
            tracker
                .find(&first, &templates, 0.91, None, 0, &mut workspace)
                .len(),
            2
        );
        let blank = RgbaImage::from_pixel(240, 160, image::Rgba([20, 20, 20, 255]));
        assert!(
            tracker
                .find(&blank, &templates, 0.91, None, 125, &mut workspace)
                .is_empty()
        );
        let shifted = paint(140, 26, &[50]);
        assert!(
            tracker
                .find(&shifted, &templates, 0.91, None, 250, &mut workspace)
                .is_empty()
        );
        assert!(
            !tracker
                .find(&shifted, &templates, 0.91, None, 2100, &mut workspace)
                .is_empty()
        );
        assert_eq!(tracker.confirmed.unwrap().0, 140);
    }

    #[test]
    fn stale_settings_never_supply_voice_evidence() {
        let frame = RgbaImage::new(240, 160);
        let hint = crate::HudLayoutHint {
            safe_zone_x: 0.55,
            safe_zone_y: 1.0,
            hud_scale: 1.03,
        };
        let mut tracker = SearchState::default();
        assert!(
            tracker
                .find(
                    &frame,
                    &standard_templates(),
                    0.91,
                    Some(hint),
                    0,
                    &mut MatcherWorkspace::default()
                )
                .is_empty()
        );
        assert!(tracker.confirmed.is_none());
    }

    #[test]
    fn explicit_voice_field_preserves_unicode_and_location_like_text() {
        let label = name_field("猫★… @ HOME", false);
        assert_eq!(label.text, "猫★… @ HOME");
    }

    #[test]
    fn constant_background_cannot_activate_speaker_gate() {
        let image = image::RgbaImage::from_pixel(200, 80, image::Rgba([32, 32, 32, 255]));
        let template = image::GrayImage::from_fn(12, 14, |x, y| {
            image::Luma([if x > y / 2 { 240 } else { 20 }])
        });
        assert!(
            find_icons(
                &image,
                &[IconTemplate::new(template)],
                0.82,
                (0, 64),
                &mut MatcherWorkspace::default()
            )
            .is_empty()
        );
    }
    #[test]
    fn location_is_a_field_not_part_of_the_display_label() {
        assert_eq!(
            name_field("猫★… @ Bombsite A", true),
            SpeakerLabel::new("猫★…")
        );
        assert_eq!(name_field("name@tag", true).text, "name@tag");
        assert_eq!(name_field("name @ tag", false).text, "name @ tag");
    }
    #[test]
    fn projection_filter_retains_exact_icons_at_band_and_frame_edges() {
        let glyph = image::GrayImage::from_fn(12, 14, |x, y| {
            image::Luma([if (x * 7 + y * 11) % 5 < 2 { 230 } else { 18 }])
        });
        let mut frame = image::RgbaImage::from_pixel(100, 60, image::Rgba([32, 32, 32, 255]));
        for y in 0..14 {
            for x in 0..12 {
                let v = glyph.get_pixel(x, y)[0];
                frame.put_pixel(20 + x, 46 + y, image::Rgba([v, v, v, 255]));
            }
        }
        let found = find_icons(
            &frame,
            &[IconTemplate::new(glyph)],
            0.99,
            (20, 21),
            &mut MatcherWorkspace::default(),
        );
        assert!(found.iter().any(|icon| icon.x == 20 && icon.y == 46));
    }
    #[test]
    fn dictionary_preserves_distinct_multiscalar_tokens_and_scalar_collisions() {
        let (tokens, expansions) = encode_dictionary("a\n🇩🇪\n🇩🇰\n\u{f0000}\n").unwrap();
        assert_eq!(tokens.len(), 4);
        assert_ne!(tokens[1], tokens[2]);
        assert_ne!(tokens[1], tokens[3]);
        assert_eq!(
            expand_tokens(&tokens.concat(), &expansions),
            "a🇩🇪🇩🇰\u{f0000}"
        );
        let decoder =
            oar_ocr_core::processors::BaseRecLabelDecode::from_string_list(Some(&tokens), false);
        let decoded = decoder.decode(&[vec![1, 2]], None, false);
        assert_eq!(expand_tokens(&decoded[0].0, &expansions), "🇩🇪🇩🇰");
    }
    #[test]
    fn hard_name_boundary_remains_uncertain_at_and_beyond_frame_edge() {
        assert_eq!(name_right_edge(60, 10, 4.0, 100), (100, true));
        assert_eq!(name_right_edge(60, 10, 8.0, 100), (100, true));
        assert_eq!(name_right_edge(60, 10, 2.0, 100), (80, true));
        assert_eq!(name_right_edge(60, 10, 0.0, 100), (100, false));
    }

    #[test]
    fn scoped_ocr_pool_bounds_nested_parallel_work() {
        use rayon::prelude::*;
        for threads in [1, 2] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                (0..64).into_par_iter().for_each(|_| {
                    assert_eq!(rayon::current_num_threads(), threads);
                    assert!(rayon::current_thread_index().unwrap() < threads);
                });
            });
        }
    }

    #[test]
    fn diagnostic_band_rejects_out_of_frame_bounds_after_resize() {
        let frame = RgbaImage::new(538, 40);
        assert!(validate_frame_band(&frame, (700, 800)).is_err());
        assert!(validate_frame_band(&frame, (500, 538)).is_ok());
        assert!(validate_frame_band(&RgbaImage::new(400, 40), (500, 538)).is_err());
    }

    #[test]
    fn misspelled_configuration_fields_are_rejected() {
        assert!(
            toml::from_str::<RecognizerConfig>("model = 'x'\ndictionary = 'y'\ndictionry = 'z'")
                .is_err()
        );
        let error = toml::from_str::<OcrConfig>(
            "recognizers = []\nruntime = 'x'\nspeaker_icon = 'y'\ncapture_interval_mss = 10",
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("capture_interval_mss"));
    }

    #[test]
    fn far_right_match_uses_band_relative_reusable_storage() {
        let glyph = image::GrayImage::from_fn(12, 14, |x, y| {
            image::Luma([if (x * 7 + y * 11) % 5 < 2 { 230 } else { 18 }])
        });
        let mut frame = RgbaImage::from_pixel(7200, 40, image::Rgba([32, 32, 32, 255]));
        for y in 0..14 {
            for x in 0..12 {
                let v = glyph.get_pixel(x, y)[0];
                frame.put_pixel(7000 + x, 26 + y, image::Rgba([v, v, v, 255]));
            }
        }
        let mut workspace = MatcherWorkspace::default();
        let templates = [IconTemplate::new(glyph)];
        let found = find_icons(&frame, &templates, 0.99, (7000, 7020), &mut workspace);
        assert!(found.iter().any(|icon| icon.x == 7000 && icon.y == 26));
        assert_eq!(workspace.gray.len(), 32 * 40);
        let pointer = workspace.gray.as_ptr();
        find_icons(&frame, &templates, 0.99, (7000, 7020), &mut workspace);
        assert_eq!(workspace.gray.as_ptr(), pointer);
    }
    #[test]
    fn standard_gate_finds_scaled_shifted_and_stacked_glyphs() {
        let mut frame = RgbaImage::from_pixel(320, 100, image::Rgba([28, 28, 28, 255]));
        for (x0, y0, height) in [(170, 10, 15), (170, 55, 22)] {
            let glyph = standard_stencil(height);
            for (x, y, value) in glyph.enumerate_pixels() {
                let v = 28 + (u16::from(value[0]) * 210 / 255) as u8;
                frame.put_pixel(x0 + x, y0 + y, image::Rgba([v, v, v, 255]));
            }
        }
        let found = find_icons(
            &frame,
            &standard_templates(),
            0.91,
            (0, 320),
            &mut MatcherWorkspace::default(),
        );
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|m| m.x == 170 && m.y == 10));
        assert!(found.iter().any(|m| m.x == 170 && m.y == 55));
    }

    #[test]
    fn standard_gate_rejects_blank_and_rectangular_nonvoice_controls() {
        let mut frame = RgbaImage::from_pixel(160, 70, image::Rgba([28, 28, 28, 255]));
        let templates = standard_templates();
        assert!(
            find_icons(
                &frame,
                &templates,
                0.91,
                (0, 160),
                &mut MatcherWorkspace::default()
            )
            .is_empty()
        );
        for y in 20..36 {
            for x in 40..52 {
                frame.put_pixel(x, y, image::Rgba([240, 240, 240, 255]));
            }
        }
        assert!(
            find_icons(
                &frame,
                &templates,
                0.91,
                (0, 160),
                &mut MatcherWorkspace::default()
            )
            .is_empty()
        );
    }
}

/// Independently authored functional loudspeaker stencil, not captured artwork.
/// Only the static body is matched; animated sound waves are deliberately excluded.
fn standard_stencil(height: u32) -> image::GrayImage {
    let width = (height * 17).div_ceil(20);
    image::GrayImage::from_fn(width, height, |x, y| {
        let mut coverage = 0_u32;
        for sy in 0..4 {
            for sx in 0..4 {
                let px = 4.0 + (x as f32 + (sx as f32 + 0.5) / 4.0) * 17.0 / width as f32;
                let py = 2.0 + (y as f32 + (sy as f32 + 0.5) / 4.0) * 20.0 / height as f32;
                let body = (5.0..=11.0).contains(&px) && (8.0..=16.0).contains(&py);
                let horn = (11.0..=20.0).contains(&px)
                    && py >= 8.0 - (px - 11.0) * 5.0 / 9.0
                    && py <= 16.0 + (px - 11.0) * 5.0 / 9.0;
                coverage += u32::from(body || horn);
            }
        }
        image::Luma([(coverage * 255 / 16) as u8])
    })
}

fn standard_templates() -> Vec<IconTemplate> {
    // Integer sizes cover ordinary HUD scaling without an installation crop.
    (10..=40)
        .map(|height| IconTemplate::new(standard_stencil(height)))
        .collect()
}

#[derive(Default)]
struct SearchState {
    confirmed: Option<(u32, u32)>,
    last_seen: u64,
    last_broad: Option<u64>,
}

impl SearchState {
    fn find(
        &mut self,
        image: &RgbaImage,
        templates: &[IconTemplate],
        threshold: f32,
        hint: Option<crate::HudLayoutHint>,
        now: u64,
        workspace: &mut MatcherWorkspace,
    ) -> Vec<IconMatch> {
        if now.saturating_sub(self.last_seen) > 2000 {
            self.confirmed = None;
        }
        let mut matches = Vec::new();
        if let Some((x, height)) = self.confirmed {
            let first = height.saturating_sub(2).max(10) as usize - 10;
            let end = (height + 2).min(40) as usize - 9;
            matches = find_icons(
                image,
                &templates[first..end],
                threshold,
                (
                    x.saturating_sub(height),
                    (x + height * 2).min(image.width()),
                ),
                workspace,
            );
        } else if let Some(hint) = hint {
            let x = (image.width() as f32 * hint.inset_fraction() / 0.55) as u32;
            let height = (24.0 * hint.hud_scale * image.height() as f32 / (1080.0 * 0.45))
                .round()
                .clamp(10.0, 40.0) as u32;
            let first = height.saturating_sub(3).max(10) as usize - 10;
            let end = (height + 3).min(40) as usize - 9;
            let band = (
                x.saturating_sub(height * 3),
                (x + height * 5).min(image.width()),
            );
            if band.0 < band.1 {
                matches = find_icons(image, &templates[first..end], threshold, band, workspace);
            }
        }
        if matches.is_empty()
            && self
                .last_broad
                .is_none_or(|last| now.saturating_sub(last) >= 2000)
        {
            self.last_broad = Some(now);
            matches = find_icons(image, templates, threshold, (0, image.width()), workspace);
        }
        if !matches.is_empty() && matches.len() <= 8 {
            let first = &matches[0];
            if matches.iter().all(|m| {
                m.x.abs_diff(first.x) <= first.height && m.height.abs_diff(first.height) <= 3
            }) {
                self.confirmed = Some((first.x, first.height));
                self.last_seen = now;
            } else {
                matches.clear();
            }
        }
        matches
    }
}

#[derive(Clone)]
struct IconMatch {
    x: u32,
    y: u32,
    height: u32,
    score: f32,
}

struct IconTemplate {
    width: u32,
    height: u32,
    centered: Vec<f64>,
    variance: f64,
    cells: Vec<(u32, u32, u32, u32, f64)>,
    projected_variance: f64,
}
impl IconTemplate {
    fn new(image: image::GrayImage) -> Self {
        let (width, height) = image.dimensions();
        let n = f64::from(width * height);
        let mean = image.as_raw().iter().map(|v| f64::from(*v)).sum::<f64>() / n;
        let centered: Vec<_> = image
            .as_raw()
            .iter()
            .map(|v| f64::from(*v) - mean)
            .collect();
        let variance = centered.iter().map(|v| v * v).sum();
        let mut cells = Vec::new();
        let mut projected_variance = 0.0;
        for cy in 0..4 {
            for cx in 0..4 {
                let (x0, x1) = (cx * width / 4, (cx + 1) * width / 4);
                let (y0, y1) = (cy * height / 4, (cy + 1) * height / 4);
                if x1 == x0 || y1 == y0 {
                    continue;
                }
                let mut sum = 0.0;
                for y in y0..y1 {
                    for x in x0..x1 {
                        sum += centered[(y * width + x) as usize];
                    }
                }
                let area = f64::from((x1 - x0) * (y1 - y0));
                projected_variance += sum * sum / area;
                cells.push((x0, y0, x1, y1, sum / area));
            }
        }
        // Test the most informative coarse cells first. Partial orthogonal
        // projections already provide a rigorous bound, without full pixel NCC.
        cells.sort_by(|a, b| {
            let energy = |&(x0, y0, x1, y1, mean): &(u32, u32, u32, u32, f64)| {
                f64::from((x1 - x0) * (y1 - y0)) * mean * mean
            };
            energy(b).total_cmp(&energy(a))
        });
        Self {
            width,
            height,
            centered,
            variance,
            cells,
            projected_variance,
        }
    }
}

#[derive(Default)]
struct MatcherWorkspace {
    gray: Vec<f64>,
    integral: Vec<f64>,
    squared: Vec<f64>,
}

/// Integral cell projections give a Cauchy-Schwarz upper bound on exact NCC.
/// Rejected windows cannot meet the threshold; no stride-based speaker misses.
fn find_icons(
    image: &RgbaImage,
    templates: &[IconTemplate],
    threshold: f32,
    band: (u32, u32),
    workspace: &mut MatcherWorkspace,
) -> Vec<IconMatch> {
    let origin = band.0.min(image.width());
    let width = image.width().min(
        band.1
            .saturating_add(templates.iter().map(|t| t.width).max().unwrap_or(0)),
    ) - origin;
    let height = image.height();
    let stride = (width + 1) as usize;
    let MatcherWorkspace {
        gray,
        integral,
        squared,
    } = workspace;
    gray.clear();
    gray.reserve((width * height) as usize);
    integral.resize(stride * (height + 1) as usize, 0.0);
    integral.fill(0.0);
    squared.resize(integral.len(), 0.0);
    squared.fill(0.0);
    for y in 0..height {
        let mut sum = 0.0;
        let mut squares = 0.0;
        for x in 0..width {
            let pixel = image.get_pixel(origin + x, y).0;
            let value = (299 * u32::from(pixel[0])
                + 587 * u32::from(pixel[1])
                + 114 * u32::from(pixel[2])) as f64
                / 1000.0;
            gray.push(value);
            sum += value;
            squares += value * value;
            let index = (y + 1) as usize * stride + x as usize + 1;
            integral[index] = integral[index - stride] + sum;
            squared[index] = squared[index - stride] + squares;
        }
    }
    let area_sum = |table: &[f64], x0: u32, y0: u32, x1: u32, y1: u32| {
        table[y1 as usize * stride + x1 as usize] + table[y0 as usize * stride + x0 as usize]
            - table[y0 as usize * stride + x1 as usize]
            - table[y1 as usize * stride + x0 as usize]
    };
    let mut matches = Vec::<IconMatch>::new();
    for template in templates {
        let (tw, th) = (template.width, template.height);
        if tw > width || th > height || template.variance < f64::from(tw * th) * 16.0 {
            continue;
        }
        let n = f64::from(tw * th);
        for y in 0..=height - th {
            'window: for x in 0..=(band.1 - origin).min(width - tw) {
                let sum = area_sum(integral, x, y, x + tw, y + th);
                let variance = (area_sum(squared, x, y, x + tw, y + th) - sum * sum / n).max(0.0);
                let denominator = (variance * template.variance).sqrt();
                if denominator < 1.0 {
                    continue;
                }
                let mut projected_product = 0.0;
                let mut projected_energy = 0.0;
                let mut template_energy = 0.0;
                for &(x0, y0, x1, y1, mean) in &template.cells {
                    let area = f64::from((x1 - x0) * (y1 - y0));
                    let centered_sum =
                        area_sum(integral, x + x0, y + y0, x + x1, y + y1) - sum * area / n;
                    projected_product += centered_sum * mean;
                    projected_energy += centered_sum * centered_sum / area;
                    template_energy += area * mean * mean;
                    let upper = projected_product
                        + ((variance - projected_energy).max(0.0)
                            * (template.variance - template_energy).max(0.0))
                        .sqrt();
                    if upper + 0.000001 < f64::from(threshold) * denominator {
                        continue 'window;
                    }
                }
                let upper = projected_product
                    + ((variance - projected_energy).max(0.0)
                        * (template.variance - template.projected_variance).max(0.0))
                    .sqrt();
                if upper + 0.000001 < f64::from(threshold) * denominator {
                    continue;
                }
                let mut product = 0.0;
                for dy in 0..th {
                    let start = ((y + dy) * width + x) as usize;
                    let template_start = (dy * tw) as usize;
                    for dx in 0..tw as usize {
                        product += gray[start + dx] * template.centered[template_start + dx];
                    }
                }
                let score = (product / denominator) as f32;
                if score < threshold {
                    continue;
                }
                let candidate = IconMatch {
                    x: origin + x,
                    y,
                    height: th,
                    score,
                };
                if let Some(existing) = matches.iter_mut().find(|other| {
                    (other.y as f32 + other.height as f32 / 2.0 - y as f32 - th as f32 / 2.0).abs()
                        < th as f32 * 0.65
                }) {
                    if score > existing.score {
                        *existing = candidate;
                    }
                } else {
                    matches.push(candidate);
                }
            }
        }
    }
    matches
}
