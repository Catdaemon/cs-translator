use super::*;
use gpui_kit::component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    message::{Message, MessageContent, MessageFooter, MessageHeader},
    message_scroller::MessageScroller,
    progress::Progress,
    searchable_list::SearchableListItem,
    shimmer::ShimmerText,
    spinner::Spinner,
    switch::Switch,
};

pub(super) fn caption_message(caption: &Caption, config: &AppConfig) -> AnyElement {
    let has_translation = !caption.translated.is_empty() && caption.translation_error.is_none();
    let pending =
        caption.translation_error.is_none() && (caption.translation_pending || !caption.is_final);
    let text = caption_display_text(caption);
    let content = if pending {
        ShimmerText::new(text.clone())
            .id(("caption-translation", caption.id))
            .w_full()
            .min_w_0()
            .into_any_element()
    } else {
        div()
            .w_full()
            .min_w_0()
            .child(text.clone())
            .into_any_element()
    };
    Message::new()
        .w_full()
        .min_w_0()
        .px_3()
        .pr_10()
        .py_2()
        .gap_1()
        .with_stack_style(div().gap_1().style().clone())
        .rounded_md()
        .bg(rgb(0x303235))
        .when(config.show_message_labels, |message| {
            message.header(
                MessageHeader::new()
                    .content_inset(false)
                    .text_xs()
                    .text_color(rgb(0xe6ab45))
                    .child(speaker_text(caption)),
            )
        })
        .content(
            MessageContent::new()
                .w_full()
                .min_w_0()
                .text_size(px(f32::from(config.caption_font_size)))
                .text_color(rgb(0xf1eee7))
                .child(content),
        )
        .when(caption.translation_error.is_some(), |message| {
            message.footer(
                MessageFooter::new()
                    .content_inset(false)
                    .text_sm()
                    .text_color(rgb(0xe9a07f))
                    .child("Untranslated"),
            )
        })
        .when(config.show_original && has_translation, |message| {
            message.footer(
                MessageFooter::new()
                    .content_inset(false)
                    .w_full()
                    .min_w_0()
                    .text_sm()
                    .text_color(rgb(0xbab6ab))
                    .child(div().w_full().min_w_0().child(caption.original.clone())),
            )
        })
        .into_any_element()
}

impl SearchableListItem for assets::Language {
    type Value = &'static str;

    fn title(&self) -> SharedString {
        self.name.into()
    }

    fn value(&self) -> &Self::Value {
        &self.code
    }

    fn display_title(&self) -> Option<AnyElement> {
        Some(language_label(self))
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        language_label(self)
    }

    fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self
                .name
                .as_bytes()
                .windows(query.len())
                .any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
            || self
                .code
                .as_bytes()
                .windows(query.len())
                .any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
    }
}

fn language_label(language: &assets::Language) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .when(language.code != "auto", |row| {
            row.child(flag(language.code))
        })
        .child(language.name)
        .into_any_element()
}

fn flag(language: &str) -> AnyElement {
    img(ui_assets::path(language))
        .w(px(24.))
        .h(px(18.))
        .flex_shrink_0()
        .rounded_sm()
        .into_any_element()
}

impl Page {
    pub(super) fn is_setup(self) -> bool {
        matches!(self, Self::Setup | Self::ChatSetup)
    }
}

pub(super) fn page_size(page: Page, display: Size<Pixels>) -> Size<Pixels> {
    let preferred = match page {
        Page::Setup | Page::ChatSetup => size(px(1080.), px(820.)),
        Page::Home | Page::Settings => size(px(860.), px(720.)),
    };
    size(
        preferred.width.min((display.width - px(48.)).max(px(320.))),
        (preferred.height + TITLEBAR_HEIGHT).min((display.height - px(96.)).max(px(360.))),
    )
}

fn status_icon(state: RunState) -> AnyElement {
    match state {
        RunState::Waiting
        | RunState::WaitingForConsole
        | RunState::Starting
        | RunState::Restarting => Spinner::new()
            .color(rgb(0xe6ab45).into())
            .into_any_element(),
        RunState::Paused => gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Pause)
            .text_color(rgb(0xbab6ab))
            .into_any_element(),
        RunState::Live => gpui_kit::component::Icon::new(gpui_kit::assets::IconName::CircleCheck)
            .text_color(rgb(0x9ac979))
            .into_any_element(),
    }
}

impl Desktop {
    fn voice_control(&self, cx: &Context<Self>) -> AnyElement {
        Switch::new("voice-translation")
            .label("Voice translation")
            .checked(self.config.voice_enabled)
            .small()
            .tooltip("Turn off to translate console chat only, without audio capture or speech recognition.")
            .on_click(cx.listener(|this, enabled: &bool, _, cx| {
                this.voice_changed(*enabled, cx);
            }))
            .into_any_element()
    }

    fn home_page(&self, cx: &Context<Self>) -> AnyElement {
        let state = self.run_state();
        let desktop = cx.weak_entity();
        let desktop_entity = desktop.clone();
        let feed = if self.captions.is_empty() {
            div()
                .p_4()
                .text_sm()
                .text_color(rgb(0xbab6ab))
                .child(if self.config.voice_enabled {
                    "Translated voice and text chat will appear here."
                } else {
                    "Translated text chat will appear here."
                })
                .into_any_element()
        } else {
            MessageScroller::new(
                "live-chat-output",
                self.message_scroller.clone(),
                move |index, _, cx| {
                    let Some(desktop) = desktop.upgrade() else {
                        return div().into_any_element();
                    };
                    let desktop = desktop.read(cx);
                    let Some(caption) = desktop
                        .captions
                        .len()
                        .checked_sub(index + 1)
                        .and_then(|index| desktop.captions.get(index))
                    else {
                        return div().into_any_element();
                    };
                    div()
                        .w_full()
                        .relative()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(caption_message(caption, &desktop.config))
                        .child(div().absolute().top_1().right_1().child(
                            desktop.caption_language_actions(caption, desktop_entity.clone()),
                        ))
                        .into_any_element()
                },
            )
            .with_row_style(div().px_2().pb_2().style().clone())
            .flex_1()
            .min_h_0()
            .w_full()
            .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .child("CS / TRANSLATOR"),
                    )
                    .child(
                        Button::new("settings")
                            .label("Settings")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page = Page::Settings;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .gap_3()
                    .h(px(32.))
                    .items_center()
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_3()
                            .h_full()
                            .px_3()
                            .rounded_md()
                            .bg(rgb(0x30302b))
                            .text_color(rgb(0xe9c27f))
                            .child(status_icon(state))
                            .child(match state {
                                RunState::Paused => "Paused",
                                RunState::Waiting => "Waiting for CS2 to launch",
                                RunState::WaitingForConsole => {
                                    "Waiting for console log · enable -condebug"
                                }
                                RunState::Starting => "Starting…",
                                RunState::Restarting => "Restarting…",
                                RunState::Live if !self.config.voice_enabled => {
                                    "Running · Text only"
                                }
                                RunState::Live => "Running",
                            }),
                    )
                    .child(
                        Button::new("start-stop")
                            .primary()
                            .w(px(96.))
                            .label(if self.desired_running {
                                "Stop"
                            } else {
                                "Start"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_running(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x4b4c47))
                    .bg(rgb(0x282a2c))
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .border_b_1()
                            .border_color(rgb(0x42433f))
                            .font_weight(FontWeight::BOLD)
                            .child("Live translations")
                            .child(self.voice_control(cx)),
                    )
                    .child(feed),
            )
            .into_any_element()
    }

    fn setup_page(&self, cx: &Context<Self>) -> AnyElement {
        use downloads::State;
        let mut content = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .child("Welcome to CS Translator"),
            )
            .child(
                div()
                    .text_color(rgb(0xbab6ab))
                    .child("First, we need to download these AI models:"),
            )
            .child(self.voice_control(cx));
        for (index, model) in self
            .downloads
            .iter()
            .flat_map(|downloads| downloads.snapshots())
            .enumerate()
        {
            if !self.config.voice_enabled && model.voice_only {
                continue;
            }
            let progress = if model.total == 0 {
                0.
            } else {
                (model.received as f32 / model.total as f32).clamp(0., 1.)
            };
            let state = match &model.state {
                State::Checking => "Checking files…",
                State::Idle => {
                    if model.received == 0 {
                        "Not downloaded"
                    } else {
                        "Download paused"
                    }
                }
                State::Downloading => "Downloading…",
                State::Stopping => "Stopping…",
                State::Verifying => "Verifying…",
                State::Installing => "Installing…",
                State::Ready => "Ready",
                State::Error(_) => "Download failed · retry",
            };
            let active = model.state.active();
            let can_toggle = !matches!(
                model.state,
                State::Checking | State::Stopping | State::Ready
            );
            let action = Button::new(("model-download", index))
                .disabled(!can_toggle)
                .label(if matches!(model.state, State::Checking) {
                    "Checking…"
                } else if matches!(model.state, State::Stopping) {
                    "Stopping…"
                } else if matches!(model.state, State::Ready) {
                    "Ready"
                } else if active {
                    "Stop"
                } else if model.received > 0 {
                    "Resume"
                } else {
                    "Start"
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(downloads) = &this.downloads {
                        downloads.toggle(index);
                    }
                    cx.notify();
                }));
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(0x2c2e30))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(model.name)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0xbab6ab))
                                    .child(model.detail),
                            )
                            .child(
                                Progress::new(("model-progress", index))
                                    .value(progress * 100.)
                                    .w_full(),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .justify_between()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(rgb(0xbab6ab))
                                    .child(format!(
                                        "{:.1} / {:.1} MB",
                                        model.received as f64 / 1_000_000.,
                                        model.total as f64 / 1_000_000.
                                    ))
                                    .child(state.to_owned()),
                            ),
                    )
                    .child(action),
            );
        }
        content = content.child(div().text_xs().text_color(rgb(0xbab6ab))
            .child("Models stay in the models folder next to the app. After setup, translation works offline."));
        if self.assets_ready {
            content = content.child(
                gpui_kit::component::select::Select::new(&self.language_select)
                    .accessibility_label("Translation target language")
                    .placeholder("Choose language")
                    .menu_width(px(320.))
                    .menu_max_h(px(240.))
                    .w_full(),
            );
        }
        let ready = self.assets_ready
            && self
                .downloads
                .as_ref()
                .is_some_and(|downloads| downloads.all_ready(self.config.voice_enabled))
            && self.asset_error.is_none()
            && !self.config_failed;
        let next = Button::new("setup-next")
            .primary()
            .label("Next")
            .text_base()
            .font_weight(FontWeight::BOLD)
            .disabled(!ready)
            .on_click(cx.listener(|this, _, _, cx| {
                this.page = Page::ChatSetup;
                cx.notify();
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.recovery_actions(content, cx))
            .child(div().flex_1().min_h_0())
            .child(
                div()
                    .flex()
                    .justify_end()
                    .pt_4()
                    .border_t_1()
                    .border_color(rgb(0x42433f))
                    .flex_shrink_0()
                    .child(next),
            )
            .into_any_element()
    }

    fn chat_setup_page(&self, cx: &Context<Self>) -> AnyElement {
        let properties = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .rounded_md()
            .bg(rgb(0x2c2e30))
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::BOLD)
                    .child("1. Open CS2 properties"),
            )
            .child(div().h(px(60.)).text_sm().text_color(rgb(0xbab6ab)).child(
                "In your Steam library, right-click Counter-Strike 2 and choose Properties…",
            ))
            .child(
                img("setup/steam-properties.png")
                    .w_full()
                    .h(px(320.))
                    .object_fit(ObjectFit::Contain),
            );
        let launch_options =
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_4()
                .p_4()
                .rounded_md()
                .bg(rgb(0x2c2e30))
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::BOLD)
                        .child("2. Add the launch option"),
                )
                .child(div().h(px(60.)).text_sm().text_color(rgb(0xbab6ab)).child(
                    "Under General → Launch Options, add -condebug. Keep any existing options.",
                ))
                .child(
                    img("setup/steam-launch-options.png")
                        .w_full()
                        .h(px(320.))
                        .object_fit(ObjectFit::Contain),
                );
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .child("Text chat setup"),
            )
            .child(
                div()
                    .text_color(rgb(0xbab6ab))
                    .child("Add -condebug to CS2’s Steam launch options to translate chat."),
            )
            .child(
                div()
                    .flex()
                    .items_stretch()
                    .gap_4()
                    .child(properties)
                    .child(launch_options),
            )
            .child(div().text_sm().text_color(rgb(0xbab6ab)).child(
                "Restart CS2 if it is already running. Voice translation needs no launch option.",
            ))
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .pt_4()
                    .border_t_1()
                    .border_color(rgb(0x42433f))
                    .flex_shrink_0()
                    .child(
                        Button::new("chat-setup-back")
                            .label("Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page = Page::Setup;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("finish-setup")
                            .primary()
                            .label("Finish setup")
                            .text_base()
                            .font_weight(FontWeight::BOLD)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.config.setup_completed = true;
                                this.save_pending = true;
                                this.page = Page::Home;
                                this.start_if_ready(cx);
                                this.save();
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.fitted_page != Some(self.page) {
            self.fitted_page = Some(self.page);
            let display = window
                .display(cx)
                .map(|display| display.bounds().size)
                .unwrap_or(size(px(1200.), px(920.)));
            let fitted = page_size(self.page, display);
            // Native resize can dispatch window events; wait until this render borrow is released.
            cx.defer_in(window, move |this, window, cx| {
                window.resize(fitted);
                window.focus(&this.keyboard_focus, cx);
            });
        }
        let page = match self.page {
            Page::Home => self.home_page(cx),
            Page::Settings => self.settings_page(cx),
            Page::Setup => self.setup_page(cx),
            Page::ChatSetup => self.chat_setup_page(cx),
        };
        div()
            .id("page")
            .track_focus(&self.keyboard_focus)
            .tab_stop(false)
            .tab_group()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x222427))
            .text_color(rgb(0xf1eee7))
            .font_family("Segoe UI")
            .child(
                TitleBar::new().h(TITLEBAR_HEIGHT).text_sm().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            img("app-icon.png")
                                .w(px(20.))
                                .h(px(20.))
                                .object_fit(ObjectFit::Contain),
                        )
                        .child("CS Translator"),
                ),
            )
            .child(div().flex_1().min_h_0().p_6().child(page))
    }
}
