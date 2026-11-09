use super::*;
use gpui_kit::assets::IconName;
use gpui_kit::component::WindowExt;
use gpui_kit::component::{Sizable, button::Button, select::Select};

fn observed_label(player: &cs_core::SpeakerLabel) -> String {
    if player.truncated {
        format!("{} (truncated label)", player.text)
    } else {
        player.text.clone()
    }
}

impl Desktop {
    pub(super) fn prepare_source_languages(
        &mut self,
        languages: &[Language],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.source_languages = std::iter::once(Language {
            code: "auto",
            name: "Auto",
        })
        .chain(
            languages
                .iter()
                .copied()
                .filter(|language| language.code != "auto"),
        )
        .collect();
        let source = self
            .source_languages
            .iter()
            .find(|language| language.code == self.config.source_language)
            .map(|language| language.code);
        self.source_select.update(cx, |state, cx| {
            state.set_items(
                SearchableVec::new(self.source_languages.clone()),
                window,
                cx,
            );
            if let Some(source) = source {
                state.set_selected_value(&source, window, cx);
            }
        });
        self.player_select.update(cx, |state, cx| {
            state.set_items(
                SearchableVec::new(self.source_languages.clone()),
                window,
                cx,
            );
        });
    }

    pub(super) fn open_language_editor(
        &mut self,
        player: Option<cs_core::SpeakerLabel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let code = match &player {
            Some(player) => self
                .player_languages
                .iter()
                .find(|tag| tag.player == *player)
                .map_or("auto", |tag| tag.language.as_str()),
            None => self.config.source_language.as_str(),
        };
        let selected = self
            .source_languages
            .iter()
            .find(|language| language.code == code)
            .map(|language| language.code);
        self.player_select.update(cx, |state, cx| {
            if let Some(code) = selected {
                state.set_selected_value(&code, window, cx);
            }
        });
        self.language_editor_player = player.clone();
        self.language_editor_valid = true;
        let select = self.player_select.clone();
        let player_label = player
            .as_ref()
            .map_or_else(|| "Default".to_owned(), observed_label);
        let initial_focus = std::cell::Cell::new(true);
        let desktop = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                select.read(cx).focus_handle(cx).focus(window, cx);
            }
            dialog
                .title("Translation source language")
                .on_close({
                    let desktop = desktop.clone();
                    move |_, _, cx| {
                        let _ = desktop.update(cx, |this, _| this.language_editor_valid = false);
                    }
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(player_label.clone())
                        .child(
                            Select::new(&select)
                                .accessibility_label("Translation source language")
                                .placeholder("Search source languages")
                                .menu_width(px(320.))
                                .menu_max_h(px(240.))
                                .w_full(),
                        ),
                )
        });
        cx.notify();
    }

    pub(super) fn set_player_language(&mut self, code: &str, cx: &mut Context<Self>) {
        let Some(player) = self.language_editor_player.clone() else {
            if self.config.source_language != code {
                self.config.source_language = code.into();
                self.translation_policy_changed(cx);
            }
            return;
        };
        let existing = self
            .player_languages
            .iter()
            .find(|tag| tag.player == player);
        if existing.map_or(code == "auto", |tag| tag.language == code) {
            return;
        }
        self.player_languages.retain(|tag| tag.player != player);
        if code != "auto" {
            if self.player_languages.len() == 256 {
                self.player_languages.remove(0);
            }
            self.player_languages.push(cs_core::PlayerLanguage {
                player,
                language: code.into(),
            });
        }
        self.translation_policy_changed(cx);
    }

    pub(super) fn source_language_controls(&self, _cx: &Context<Self>) -> AnyElement {
        Select::new(&self.source_select)
            .accessibility_label("Global translation source language")
            .placeholder("Choose source language")
            .menu_width(px(320.))
            .menu_max_h(px(240.))
            .w_full()
            .into_any_element()
    }

    pub(super) fn caption_language_actions(
        &self,
        caption: &Caption,
        entity: WeakEntity<Self>,
    ) -> AnyElement {
        let player = match caption.speakers.as_slice() {
            [player] => Some(player.clone()),
            _ => None,
        };
        Button::new(("caption-language", caption.id))
            .icon(IconName::Languages)
            .accessibility_label("Correct translation source language")
            .tooltip("Correct translation source language")
            .small()
            .on_click(move |_, window, cx| {
                if let Some(entity) = entity.upgrade() {
                    entity.update(cx, |this, cx| {
                        this.open_language_editor(player.clone(), window, cx)
                    });
                }
            })
            .into_any_element()
    }
}
