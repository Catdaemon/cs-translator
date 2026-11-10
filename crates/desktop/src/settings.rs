use super::*;
use gpui_kit::component::{
    alert::Alert,
    button::Button,
    form::{Field, Form},
    select::Select,
    setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
};

fn preview_caption() -> &'static Caption {
    static SAMPLE: std::sync::LazyLock<Caption> = std::sync::LazyLock::new(|| Caption {
        id: 0,
        start_ms: 0,
        end_ms: 0,
        channel: "voice".into(),
        speakers: vec![cs_core::SpeakerLabel::new("Teammate")],
        original: "Deux adversaires en B. Allons-y ensemble.".into(),
        translated: "Two opponents at B. Let's go together.".into(),
        translation_pending: false,
        translation_error: None,
        language: Some("fr".into()),
        is_final: true,
    });
    &SAMPLE
}

impl Desktop {
    fn appearance_preview(&self) -> AnyElement {
        div()
            .w_full()
            .min_w_0()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x45464a))
            .bg(rgb(0x282a2c))
            .child(pages::caption_message(preview_caption(), &self.config))
            .into_any_element()
    }

    fn overlay_preview(&self) -> AnyElement {
        let content = if self.config.overlay_enabled {
            div()
                .size_full()
                .flex()
                .flex_col()
                .justify_end()
                .child(overlay_caption(
                    preview_caption(),
                    OverlayAppearance::from(&self.config),
                ))
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(rgb(0xf1eee7))
                .child("Overlay hidden")
        };
        div()
            .relative()
            .w_full()
            .max_w(px(self.config.overlay_width))
            .h(px(164.))
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(0x646b71))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_1_2()
                    .h_full()
                    .bg(rgb(0x8b9095)),
            )
            .child(div().relative().size_full().child(content))
            .into_any_element()
    }

    fn settings_toggle(
        &self,
        title: &'static str,
        description: &'static str,
        aliases: &[&'static str],
        get: fn(&AppConfig) -> bool,
        set: fn(&mut Self, bool, &mut Context<Self>),
        cx: &Context<Self>,
    ) -> SettingItem {
        let read = cx.weak_entity();
        let write = read.clone();
        SettingItem::new(
            title,
            SettingField::switch(
                move |cx| {
                    read.upgrade()
                        .is_some_and(|this| get(&this.read(cx).config))
                },
                move |value, cx| {
                    if let Some(this) = write.upgrade() {
                        this.update(cx, |this, cx| {
                            if get(&this.config) != value {
                                set(this, value, cx);
                            }
                        });
                    }
                },
            ),
        )
        .description(description)
        .keywords(aliases.iter().copied())
    }

    fn settings_number(
        title: &'static str,
        description: &'static str,
        options: NumberFieldOptions,
        get: fn(&AppConfig) -> f64,
        set: fn(&mut AppConfig, f64),
        changed: fn(&mut Self, &mut Context<Self>),
        cx: &Context<Self>,
    ) -> SettingItem {
        let read = cx.weak_entity();
        let write = read.clone();
        SettingItem::new(
            title,
            SettingField::number_input(
                options,
                move |cx| read.upgrade().map_or(0., |this| get(&this.read(cx).config)),
                move |value, cx| {
                    if let Some(this) = write.upgrade() {
                        this.update(cx, |this, cx| {
                            if !value.is_finite() || get(&this.config) == value {
                                return;
                            }
                            let previous = get(&this.config);
                            set(&mut this.config, value);
                            if get(&this.config) == previous {
                                return;
                            }
                            changed(this, cx);
                        });
                    }
                },
            ),
        )
        .description(description)
    }

    fn overlay_position_picker(&self, cx: &Context<Self>) -> AnyElement {
        let (mut horizontal, mut vertical, mut width, mut height) = (0.5, 0.5, 0.25, 0.14);
        if let Some(display) = cx.primary_display() {
            let bounds = display.bounds();
            let screen_width = f32::from(bounds.size.width).max(1.);
            let screen_height = f32::from(bounds.size.height).max(1.);
            horizontal = ((self.config.overlay_x - f32::from(bounds.origin.x) - 24.)
                / (screen_width - self.config.overlay_width - 48.).max(1.))
            .clamp(0., 1.);
            vertical = ((self.config.overlay_y - f32::from(bounds.origin.y) - 24.)
                / (screen_height - self.config.overlay_height - 48.).max(1.))
            .clamp(0., 1.);
            width = (self.config.overlay_width / screen_width).clamp(0.08, 1.);
            height = (self.config.overlay_height / screen_height).clamp(0.06, 1.);
        }
        let mut monitor = div()
            .relative()
            .w(px(300.))
            .h(px(168.))
            .bg(rgb(0x17191d))
            .border_2()
            .border_color(rgb(0x55504a))
            .rounded_md()
            .child(
                div()
                    .absolute()
                    .left(px(4. + horizontal * (288. - 288. * width)))
                    .top(px(4. + vertical * (156. - 156. * height)))
                    .w(px(288. * width))
                    .h(px(156. * height))
                    .bg(rgba(0xf4b54455))
                    .border_1()
                    .border_color(rgb(0xf4b544))
                    .rounded_sm(),
            );
        for (id, label, glyph, x, y, left, top) in [
            ("top-left", "Top left", "↖", 0., 0., 8., 8.),
            ("top-right", "Top right", "↗", 1., 0., 264., 8.),
            ("bottom-left", "Bottom left", "↙", 0., 1., 8., 136.),
            ("bottom-right", "Bottom right", "↘", 1., 1., 264., 136.),
            ("centre", "Centre", "•", 0.5, 0.5, 136., 72.),
        ] {
            let selected = (horizontal - x).abs() < 0.025 && (vertical - y).abs() < 0.025;
            monitor = monitor.child(
                div().absolute().left(px(left)).top(px(top)).child(
                    Button::new(id)
                        .label(glyph)
                        .tooltip(label)
                        .w(px(28.))
                        .h(px(24.))
                        .p_0()
                        .bg(rgb(0x090b0e))
                        .accessibility_label(format!(
                            "Position overlay {label}{}",
                            if selected { ", selected" } else { "" }
                        ))
                        .when(selected, |button| button.bg(rgb(0x8a591d)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.overlay_position(x, y, cx);
                            this.save();
                            cx.notify();
                        })),
                ),
            );
        }
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .child(monitor)
            .child(div().w(px(14.)).h(px(10.)).bg(rgb(0x55504a)))
            .child(div().w(px(86.)).h(px(4.)).bg(rgb(0x55504a)).rounded_sm())
            .child(
                div()
                    .w(px(300.))
                    .text_center()
                    .text_xs()
                    .child("Amber rectangle shows the overlay"),
            )
            .into_any_element()
    }

    pub(super) fn settings_page(&self, cx: &Context<Self>) -> AnyElement {
        let pages = self.settings_pages.get_or_init(|| {
        let desktop = cx.weak_entity();
        let language = SettingItem::new(
            "Translation language",
            SettingField::render(move |_, _, cx| {
                let Some(desktop) = desktop.upgrade() else {
                    return div().into_any_element();
                };
                desktop.update(cx, |this, _| {
                    Form::vertical()
                        .child(
                            Field::new().child(
                                Select::new(&this.language_select)
                                    .accessibility_label("Translation target language")
                                    .placeholder("Choose language")
                                    .menu_width(px(320.))
                                    .menu_max_h(px(240.))
                                    .w_full(),
                            ),
                        )
                        .into_any_element()
                })
            }),
        )
        .description("Language used for translated captions.")
        .keywords(["target", "translate to", "language", "locale"])
        .layout(Axis::Vertical);
        let desktop = cx.weak_entity();
        let source_languages = SettingItem::new(
            "Default source language",
            SettingField::render(move |_, _, cx| {
                let Some(desktop) = desktop.upgrade() else { return div().into_any_element(); };
                desktop.update(cx, |this, cx| this.source_language_controls(cx))
            }),
        ).description("Default source for unknown or ambiguous speakers. Caption corrections last only for the current feed.")
            .keywords(["source", "language", "auto", "detection"])
            .layout(Axis::Vertical);
        let read = cx.weak_entity();
        let write = read.clone();
        let decoding = SettingItem::new("Translation decoding", SettingField::dropdown(
            vec![("1".into(), "Greedy (1)".into()), ("2".into(), "Beam search (2)".into())],
            move |cx| read.upgrade().map_or_else(|| "1".into(), |this| this.read(cx).config.translation_beam_size.to_string().into()),
            move |value, cx| {
                let beam = match value.as_ref() { "1" => 1, "2" => 2, _ => return };
                if let Some(this) = write.upgrade() {
                    this.update(cx, |this, cx| {
                        if this.config.translation_beam_size != beam {
                            this.config.translation_beam_size = beam;
                            this.inference_changed(cx);
                        }
                    });
                }
            },
        )).description("Greedy selects one translation candidate. Beam search compares two candidates and uses more CPU.")
            .keywords(["beam", "greedy", "quality", "decoding", "speed"]);
        let translation = SettingPage::new("Translation").resettable(false).group(
            SettingGroup::new().item(language).item(source_languages).item(decoding).item(Self::settings_number(
                "Partial update cadence (ms)", "Time between translation updates while someone is speaking. Higher values reduce CPU work; 0 translates every revision. Final text skips this delay.",
                NumberFieldOptions { min: 0., max: 5000., step: 100. },
                |c| c.partial_translation_interval_ms as f64,
                |c, v| c.partial_translation_interval_ms = v.round().clamp(0., 5000.) as _, Self::translation_policy_changed, cx,
            ).keywords(["partial", "cadence", "interval", "latency", "updates"])),
        );
        let appearance_desktop = cx.weak_entity();
        let appearance = SettingPage::new("Appearance").resettable(false).group(
            SettingGroup::new()
                .item(self.settings_toggle(
                    "Original text in feed",
                    "Show the source text beneath translated captions in the main feed.",
                    &["original", "source", "bilingual", "feed"],
                    |c| c.show_original,
                    |this, value, cx| {
                        this.config.show_original = value;
                        this.feed_changed(cx);
                    },
                    cx,
                ))
                .item(self.settings_toggle(
                    "Message labels",
                    "Show player names and chat or voice labels above captions.",
                    &["speaker", "channel", "metadata", "names", "labels"],
                    |c| c.show_message_labels,
                    |this, value, cx| {
                        this.config.show_message_labels = value;
                        this.feed_changed(cx);
                    },
                    cx,
                ))
                .item(
                    Self::settings_number(
                        "Caption font size (px)",
                        "Main-feed caption text size, from 12 to 24 pixels.",
                        NumberFieldOptions {
                            min: 12.,
                            max: 24.,
                            step: 1.,
                        },
                        |c| c.caption_font_size as f64,
                        |c, v| c.caption_font_size = v.round().clamp(12., 24.) as u8,
                        Self::feed_changed,
                        cx,
                    )
                    .keywords(["font", "size", "text", "readability"]),
                )
                .item(
                    SettingItem::new(
                        "Caption preview",
                        SettingField::render(move |_, _, cx| {
                            let Some(desktop) = appearance_desktop.upgrade() else {
                                return div().into_any_element();
                            };
                            desktop.update(cx, |this, _| this.appearance_preview())
                        }),
                    )
                    .description("Sample message using your main-feed appearance.")
                    .keywords(["preview", "sample", "appearance", "caption"])
                    .layout(Axis::Vertical),
                ),
        );
        let position_desktop = cx.weak_entity();
        let position = SettingItem::new(
            "Overlay position",
            SettingField::render(move |_, _, cx| {
                let Some(desktop) = position_desktop.upgrade() else {
                    return div().into_any_element();
                };
                desktop.update(cx, |this, cx| this.overlay_position_picker(cx))
            }),
        )
        .description("Place the overlay at a corner or the centre of the primary monitor.")
        .keywords([
            "position", "monitor", "screen", "corner", "centre", "center", "anchor",
        ])
        .layout(Axis::Vertical);
        let move_desktop = cx.weak_entity();
        let movement = SettingItem::new(
            "Free overlay movement",
            SettingField::render(move |_, _, cx| {
                let Some(desktop) = move_desktop.upgrade() else {
                    return div().into_any_element();
                };
                desktop.update(cx, |this, cx| {
                    Button::new("move-overlay")
                        .label(if this.locked {
                            "Move overlay"
                        } else {
                            "Done moving"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.move_overlay(this.locked, cx);
                            this.save();
                            cx.notify();
                        }))
                        .into_any_element()
                })
            }),
        )
        .description(
            "Drag the overlay to any position. Choose Done moving to make it click-through again.",
        )
        .keywords(["drag", "move", "unlock", "lock", "free", "click-through"]);
        let overlay_desktop = cx.weak_entity();
        let overlay = SettingPage::new("Overlay").resettable(false).group(SettingGroup::new()
            .item(self.settings_toggle("Show overlay", "Show translated captions in a floating desktop window.", &["visibility", "enabled", "hide", "overlay"],
                |c| c.overlay_enabled, |this, value, cx| {
                    if value != this.overlay.is_some() {
                        this.overlay(cx);
                    }
                    if !value {
                        this.config.overlay_enabled = false;
                        this.save_pending = true;
                    }
                    this.save();
                    cx.notify();
                }, cx))
            .item(position).item(movement)
            .item(self.settings_toggle("Original text in overlay", "Include source text below translated overlay captions.", &["original", "source", "bilingual", "overlay"],
                |c| c.overlay_show_original, |this, value, cx| { this.config.overlay_show_original = value; this.overlay_appearance_changed(cx); }, cx))
            .item(Self::settings_number("Overlay font size (px)", "Overlay caption text size, from 10 to 22 pixels.",
                NumberFieldOptions { min: 10., max: 22., step: 1. }, |c| c.overlay_font_size as f64,
                |c, v| c.overlay_font_size = v.round().clamp(10., 22.) as u8, Self::overlay_appearance_changed, cx).keywords(["overlay", "font", "size", "text"]))
            .item(Self::settings_number("Overlay background opacity (%)", "Background opacity from 10% to 90%. Higher values make captions easier to read; lower values show more of the game.",
                NumberFieldOptions { min: 10., max: 90., step: 5. }, |c| c.overlay_opacity as f64,
                |c, v| c.overlay_opacity = v.round().clamp(10., 90.) as u8, Self::overlay_appearance_changed, cx).keywords(["opacity", "transparency", "background", "alpha"]))
            .item(Self::settings_number("Message expiry (seconds)", "Hide overlay messages this many seconds after their last update. Main-feed history is kept.",
                NumberFieldOptions { min: 1., max: 60., step: 1. }, |c| c.overlay_message_expiry_secs as f64,
                |c, v| c.overlay_message_expiry_secs = v.round().clamp(1., 60.) as u16, Self::expiry_changed, cx).keywords(["expiry", "expire", "duration", "timeout", "seconds", "disappear"]))
            .item(SettingItem::new("Overlay preview", SettingField::render(move |_, _, cx| {
                let Some(desktop) = overlay_desktop.upgrade() else {
                    return div().into_any_element();
                };
                desktop.update(cx, |this, _| this.overlay_preview())
            }))
                .description("Sample overlay over a two-tone background to show transparency.")
                .keywords(["preview", "sample", "overlay", "opacity", "appearance"])
                .layout(Axis::Vertical)));
        let diagnostics_desktop = cx.weak_entity();
        let performance = SettingPage::new("Performance").resettable(false).group(SettingGroup::new()
            .item(Self::settings_number("CPU threads", "CPU thread limit for speech recognition and translation, from 1 to 16. Lower values leave more CPU capacity for the game.",
                NumberFieldOptions { min: 1., max: 16., step: 1. }, |c| c.threads as f64,
                |c, v| c.threads = v.round().clamp(1., 16.) as _, Self::inference_changed, cx).keywords(["cpu", "threads", "cores", "performance", "processor"]))
            .item(self.settings_toggle("Speech beam search", "Consider four speech hypotheses instead of greedy decoding. Uses more CPU; recognition improvements depend on the audio.", &["speech", "recognition", "accuracy", "beam", "decoder"],
                |c| c.speech_beam_search, |this, value, cx| { this.config.speech_beam_search = value; this.inference_changed(cx); }, cx))
            .item(self.settings_toggle("Background inference priority", "Run speech recognition and translation at Below Normal CPU priority to favor the game. Off uses normal priority.", &["priority", "background", "scheduling", "latency", "performance"],
                |c| c.background_inference, |this, value, cx| { this.config.background_inference = value; this.inference_changed(cx); }, cx))
            .item(SettingItem::new("Capture diagnostics", SettingField::render(move |_, _, cx| {
                let Some(desktop) = diagnostics_desktop.upgrade() else {
                    return div().into_any_element();
                };
                desktop.update(cx, |this, _| {
                    let [audio, hud, chat, status, coalesced] = this.capture_totals;
                    div().text_sm().text_color(rgb(0xbab6ab))
                        .child(format!("Audio packets lost: {audio} · Speaker observations lost: {hud} · Speaker frames replaced: {coalesced} · Chat messages lost: {chat} · Diagnostics lost: {status}"))
                        .into_any_element()
                })
            })).keywords(["capture", "diagnostics", "drops", "HUD", "frames", "audio"]).layout(Axis::Vertical)));
        let speakers = SettingPage::new("Speaker detection").resettable(false).group(
            SettingGroup::new()
                .item(self.settings_toggle(
                    "Enable speaker detection",
                    "Show player names on voice captions.",
                    &["speaker", "attribution", "voice", "OCR", "enable", "names"],
                    |c| c.hud_enabled,
                    |this, enabled, cx| this.speaker_enabled(enabled, cx),
                    cx,
                )),
        );
        let info = |title, description: String, aliases: Vec<&'static str>| {
            SettingItem::new(title, SettingField::render(|_, _, _| div()))
                .description(description)
                .keywords(aliases)
                .layout(Axis::Vertical)
        };
        let mut about_group = SettingGroup::new();
        if self.managed_models {
            let desktop = cx.weak_entity();
            about_group = about_group.item(SettingItem::new("Re-do setup", SettingField::render(move |_, _, cx| {
                let Some(desktop) = desktop.upgrade() else { return div().into_any_element(); };
                desktop.update(cx, |_, cx| Button::new("model-setup").label("Re-do setup")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.locked { this.move_overlay(false, cx); }
                        this.stop();
                        this.config.setup_completed = false;
                        this.save_pending = true;
                        this.page = Page::Setup;
                        this.save();
                        cx.notify();
                    })).into_any_element())
            })).description("Return to model downloads and text-chat setup. Existing downloaded model files are retained.")
                .keywords(["setup", "onboarding", "download", "models", "redo"]));
        }
        let desktop = cx.weak_entity();
        about_group = about_group
            .item(SettingItem::new("Open-source licenses", SettingField::render(move |_, _, cx| {
                let Some(desktop) = desktop.upgrade() else { return div().into_any_element(); };
                desktop.update(cx, |_, cx| Button::new("open-source-licenses")
                    .label("Open-source licenses")
                    .on_click(cx.listener(|this, _, _, cx| this.open_licenses(cx)))
                    .into_any_element())
            }))
                .description("Browse dependency licenses, copyright notices, and model terms.")
                .keywords(["license", "licenses", "open source", "dependencies", "copyright", "notices"]))
            .item(info("Models and runtime", format!("Speech manifest (voice mode): {}\nTranslation model: {}\nSpeech engine (voice mode): sherpa-onnx\nTranslation engine: CTranslate2 (INT8, CPU)\nLanguage detection: fastText", self.config.asr_manifest.display(), self.config.translation_model.display()), vec!["model", "runtime", "nemotron", "m2m100", "sherpa", "ctranslate2"]))
            .item(info("Audio and chat sources", "Text chat is read from CS2's console log. The main-screen Voice translation switch enables game-audio capture and speech recognition; turning it off also stops speaking-indicator capture/OCR. Supported speech languages depend on the installed speech model.".into(), vec!["source", "audio", "language detection", "chat", "recognition", "voice"]))
            .item(info("Preferences and model files", format!("Preferences: {}\nApp folder: {}\nSettings are saved automatically. Model downloads and partial files are stored beside the app.", self.config_path.display(), self.asset_root.display()), vec!["preferences", "config", "toml", "folder", "downloads", "files"]))
            .item(info("Local-only processing", "Speech recognition and translation run on this computer. Internet access is needed only to download models during setup.".into(), vec!["privacy", "offline", "local", "network"]));
        vec![
            translation,
            appearance,
            overlay,
            speakers,
            performance,
            SettingPage::new("About").resettable(false).group(about_group),
        ]
        });
        let mut settings = Settings::new("translator-settings")
            .sidebar_width(px(184.))
            .sidebar_size_range(px(176.)..px(240.));
        for page in pages {
            settings = settings.page(page.clone());
        }
        let content = div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .child("Settings"),
                    )
                    .child(Button::new("back").label("Back").on_click(cx.listener(
                        |this, _, _, cx| {
                            if !this.locked {
                                this.move_overlay(false, cx);
                            }
                            this.page = Page::Home;
                            this.save();
                            cx.notify();
                        },
                    ))),
            )
            .child(div().flex_1().min_h_0().child(settings));
        self.settings_errors(content, cx).into_any_element()
    }

    pub(super) fn settings_errors(&self, mut content: Div, cx: &Context<Self>) -> Div {
        if let Some(error) = &self.asset_error {
            content = content.child(Alert::error("asset-error", error.clone()));
        }
        if let Some((message, error)) = &self.capture_notice {
            content = content.child(if *error {
                Alert::error("capture-loss", message.clone())
            } else {
                Alert::warning("capture-loss", message.clone())
            });
        }
        if let Some(message) = &self.capture_border_warning {
            content = content.child(Alert::warning("capture-border", message.clone()));
        }
        if let Some((error, true)) = &self.speaker_status {
            content = content.child(Alert::error("speaker-error", error.clone()));
        }
        if let Some((message, _)) = self.messages.iter().find(|(_, error)| *error) {
            content = content.child(Alert::error("runtime-error", message.clone()));
        }
        if self.config_failed {
            content = content.child(
                Button::new("reset-preferences")
                    .label("Restore defaults (backs up preferences)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.recover_preferences();
                        this.save();
                        cx.notify();
                    })),
            );
        }
        content
    }
}
