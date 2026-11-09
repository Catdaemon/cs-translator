#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod assets;
mod downloads;
mod language_editor;
mod licenses;
mod overlay_messages;
mod pages;
mod session;
mod settings;
mod ui_assets;
mod worker;

use self::assets::Language;
use anyhow::Result;
use clap::Parser;
use crossbeam_channel::{Receiver, Sender, bounded};
use cs_core::{AppConfig, AppEvent, Caption};
use cs_platform::GameSource;
use gpui_kit::component::{
    Theme, ThemeMode, TitleBar,
    message_scroller::MessageScrollerState,
    searchable_list::SearchableVec,
    select::{SelectEvent, SelectState},
};
use gpui_kit::{prelude::*, *};
use overlay_messages::OverlayMessages;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const TITLEBAR_HEIGHT: Pixels = px(32.);

#[derive(Parser)]
#[command(about = "Local CS2 voice and chat translator — connects automatically")]
struct Args {
    /// Optional diagnostic configuration; normal users need no configuration file.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Replay a local recording instead of capturing CS2 (diagnostics).
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Start paused instead of automatically connecting.
    #[arg(long)]
    paused: bool,
    #[arg(long)]
    exit_after: Option<f64>,
    #[arg(long, hide = true)]
    session_worker: bool,
    /// Print embedded build provenance without initializing the application.
    #[arg(long)]
    build_info: bool,
}

enum UiResult {
    Prepared(
        std::result::Result<Vec<Language>, String>,
        Option<String>,
        bool,
    ),
    Source(std::result::Result<Option<GameSource>, String>),
    Saved(std::result::Result<(), String>),
    Recovered(std::result::Result<(), String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Setup,
    ChatSetup,
    Home,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunState {
    Paused,
    Waiting,
    WaitingForConsole,
    Starting,
    Restarting,
    Live,
}

struct Desktop {
    config: AppConfig,
    config_path: PathBuf,
    asset_root: PathBuf,
    replay: Option<PathBuf>,
    source: Option<GameSource>,
    explicit_log: Option<PathBuf>,
    language_select: Entity<SelectState<SearchableVec<Language>>>,
    _language_subscription: Subscription,
    source_select: Entity<SelectState<SearchableVec<Language>>>,
    player_select: Entity<SelectState<SearchableVec<Language>>>,
    language_editor_player: Option<cs_core::SpeakerLabel>,
    player_languages: Vec<cs_core::PlayerLanguage>,
    language_editor_valid: bool,
    source_languages: Vec<Language>,
    _source_subscription: Subscription,
    _player_subscription: Subscription,
    message_scroller: Entity<MessageScrollerState>,
    page: Page,
    keyboard_focus: FocusHandle,
    fitted_page: Option<Page>,
    settings_pages: std::cell::OnceCell<Vec<gpui_kit::component::setting::SettingPage>>,
    downloads: Option<downloads::Manager>,
    managed_models: bool,
    speaker_status: Option<(String, bool)>,
    preparing_assets: bool,
    first_run: bool,
    assets_ready: bool,
    session_ready: bool,
    asset_error: Option<String>,
    config_failed: bool,
    desired_running: bool,
    restarting: bool,
    save_pending: bool,
    saving: bool,
    final_settings: Arc<Mutex<Option<(PathBuf, AppConfig)>>>,
    session: Option<session::Session>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    ui_tx: Sender<UiResult>,
    ui_rx: Receiver<UiResult>,
    captions: VecDeque<Caption>,
    overlay_feed: Entity<OverlayMessages>,
    overlay: Option<WindowHandle<Overlay>>,
    license_browser: Option<AnyWindowHandle>,
    locked: bool,
    messages: VecDeque<(String, bool)>,
    started: Instant,
    exit_after: Option<f64>,
    shutdown: session::ShutdownWorkers,
}

fn observe(tx: Sender<UiResult>, shutdown: &session::ShutdownWorkers) {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let worker = thread::spawn(move || {
        let mut previous = None;
        while !stopped.load(Ordering::Relaxed) {
            let current = cs_platform::discover_cs2_source().map_err(|e| format!("{e:#}"));
            if previous.as_ref() != Some(&current)
                && tx
                    .send_timeout(
                        UiResult::Source(current.clone()),
                        Duration::from_millis(100),
                    )
                    .is_ok()
            {
                previous = Some(current);
            }
            for _ in 0..8 {
                if stopped.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
    });
    shutdown.lock().unwrap().push((stop, worker));
}

impl Desktop {
    fn message(&mut self, text: impl Into<String>, error: bool) {
        if error {
            self.messages.push_front((text.into(), true));
            self.messages.truncate(12);
        }
    }

    fn save(&mut self) {
        if self.config_failed || self.saving || !self.save_pending {
            return;
        }
        let config = self.saved_config();
        let path = self.config_path.clone();
        let tx = self.ui_tx.clone();
        self.save_pending = false;
        self.saving = true;
        let stop = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || {
            let result = config
                .save(&path)
                .map_err(|e| format!("Save preferences: {e:#}"));
            let _ = tx.send_timeout(UiResult::Saved(result), Duration::from_secs(1));
        });
        self.shutdown.lock().unwrap().push((stop, worker));
    }

    fn recover_preferences(&mut self) {
        if self.saving {
            return;
        }
        self.saving = true;
        let config = self.saved_config();
        let path = self.config_path.clone();
        let tx = self.ui_tx.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                if path.exists() {
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos();
                    std::fs::copy(&path, path.with_extension(format!("{stamp}.backup.toml")))?;
                }
                config.save(&path)
            })()
            .map_err(|e| format!("Restore preferences: {e:#}"));
            let _ = tx.send_timeout(UiResult::Recovered(result), Duration::from_secs(1));
        });
        self.shutdown.lock().unwrap().push((stop, worker));
    }

    fn stop(&mut self) {
        self.session_ready = false;
        if let Some(session) = &self.session {
            session.stop.store(true, Ordering::Relaxed);
        }
    }

    fn start_if_ready(&mut self, cx: &mut Context<Self>) {
        if !self.desired_running
            || !self.assets_ready
            || self.asset_error.is_some()
            || self.page.is_setup()
            || self.config_failed
            || self.session.is_some()
        {
            return;
        }
        if self.replay.is_none() {
            if let Some(source) = &self.source {
                self.config.process_id = Some(source.process_id);
                self.config.capture_window = Some(source.window);
                self.config.console_log = self
                    .explicit_log
                    .clone()
                    .or_else(|| source.console_log.clone());
            } else if self.explicit_log.is_some() {
                self.config.process_id = None;
                self.config.capture_window = None;
                self.config.console_log = self.explicit_log.clone();
            } else {
                return;
            }
        }
        if !self.config.voice_enabled && self.config.console_log.is_none() {
            return;
        }
        if let Err(error) = self.config.validate() {
            self.desired_running = false;
            self.message(format!("{error:#}"), true);
            return;
        }
        self.restarting = false;
        self.captions.clear();
        self.player_languages.clear();
        self.language_editor_player = None;
        self.language_editor_valid = false;
        self.speaker_status = None;
        self.overlay_feed.update(cx, |messages, cx| {
            messages.clear();
            cx.notify();
        });
        self.message_scroller
            .update(cx, |state, cx| state.reset(0, cx));
        self.session_ready = false;
        self.session = Some(session::start(
            self.config.clone(),
            self.replay.clone(),
            self.tx.clone(),
            self.shutdown.clone(),
        ));
    }

    fn toggle_running(&mut self, cx: &mut Context<Self>) {
        self.desired_running = !self.desired_running;
        if self.desired_running {
            self.start_if_ready(cx);
        } else {
            self.restarting = false;
            self.stop();
        }
    }

    fn inference_changed(&mut self, cx: &mut Context<Self>) {
        self.save_pending = true;
        if self.session.is_some() {
            self.restarting = true;
            self.session_ready = false;
            self.stop();
        } else {
            self.start_if_ready(cx);
        }
        cx.notify();
    }

    fn speaker_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled && !self.config.voice_enabled {
            self.message("Enable Voice translation first.", true);
        } else {
            self.config.hud_enabled = enabled;
            self.speaker_status = None;
            self.inference_changed(cx);
        }
        cx.notify();
    }

    fn voice_changed(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.config.voice_enabled == enabled {
            return;
        }
        self.config.voice_enabled = enabled;
        self.assets_ready = false;
        self.asset_error = None;
        if let Some(downloads) = &self.downloads {
            downloads.set_voice_enabled(enabled);
            if !downloads.all_ready(enabled) {
                self.page = Page::Setup;
            }
        }
        self.inference_changed(cx);
    }

    fn feed_changed(&mut self, cx: &mut Context<Self>) {
        self.save_pending = true;
        let len = self.captions.len();
        self.message_scroller
            .update(cx, |state, cx| state.remeasure_items(0..len, cx));
        cx.notify();
    }

    fn expiry_changed(&mut self, cx: &mut Context<Self>) {
        self.save_pending = true;
        self.overlay_feed.update(cx, |messages, cx| {
            messages.set_lifetime(
                Duration::from_secs(u64::from(self.config.overlay_message_expiry_secs)),
                Instant::now(),
            );
            cx.notify();
        });
        cx.notify();
    }

    fn overlay_appearance_changed(&mut self, cx: &mut Context<Self>) {
        self.save_pending = true;
        if let Some(overlay) = self.overlay {
            let appearance = OverlayAppearance::from(&self.config);
            let _ = overlay.update(cx, |overlay, _, cx| {
                overlay.appearance = appearance;
                cx.notify();
            });
        }
        cx.notify();
    }

    fn translation_policy_changed(&mut self, cx: &mut Context<Self>) {
        self.save_pending = true;
        if let Some(session) = &self.session
            && let Err(error) = session.update_translation(&self.config, &self.player_languages)
        {
            self.message(format!("Update translation policy: {error:#}"), true);
        }
        cx.notify();
    }

    fn language(&mut self, code: &'static str, cx: &mut Context<Self>) {
        if self.config.target_language == code {
            return;
        }
        self.config.target_language = code.into();
        self.translation_policy_changed(cx);
    }

    fn overlay(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.overlay.take() {
            if let Ok(bounds) = handle.update(cx, |_, window, _| {
                let bounds = window.bounds();
                window.remove_window();
                bounds
            }) {
                self.config.overlay_x = f32::from(bounds.origin.x);
                self.config.overlay_y = f32::from(bounds.origin.y);
                self.config.overlay_width = f32::from(bounds.size.width);
                self.config.overlay_height = f32::from(bounds.size.height);
            }
            self.config.overlay_enabled = false;
            self.save_pending = true;
            return;
        }
        let feed = self.overlay_feed.clone();
        let locked = self.locked;
        let appearance = OverlayAppearance::from(&self.config);
        let desktop = cx.weak_entity();
        let origin = point(px(self.config.overlay_x), px(self.config.overlay_y));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                origin,
                size(
                    px(self.config.overlay_width),
                    px(self.config.overlay_height),
                ),
            ))),
            titlebar: None,
            kind: WindowKind::PopUp,
            focus: false,
            is_movable: !locked,
            is_resizable: !locked,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };
        match cx.open_window(options, move |window, cx| {
            let result = (|| -> Result<()> {
                let handle = HasWindowHandle::window_handle(window)
                    .map_err(|e| anyhow::anyhow!("Read overlay handle: {e:?}"))?
                    .as_raw();
                match handle {
                    RawWindowHandle::Win32(h) => cs_platform::make_overlay(
                        h.hwnd.get(),
                        locked,
                        (
                            (f32::from(origin.x) * window.scale_factor()).round() as i32,
                            (f32::from(origin.y) * window.scale_factor()).round() as i32,
                        ),
                    ),
                    _ => anyhow::bail!("External overlay currently requires Windows"),
                }
            })();
            cx.new(|cx| {
                let bounds_subscription = (!locked).then(|| {
                    cx.observe_window_bounds(window, move |_, window, cx| {
                        let source = Window::window_handle(window);
                        let bounds = window.bounds();
                        let desktop = desktop.clone();
                        cx.defer(move |cx| {
                            let _ = desktop.update(cx, |this, cx| {
                                this.overlay_moved(source, bounds, cx);
                            });
                        });
                    })
                });
                let feed_subscription = cx.observe(&feed, |this: &mut Overlay, _, cx| {
                    this.schedule_expiry(cx);
                    cx.notify();
                });
                let mut overlay = Overlay {
                    feed,
                    locked,
                    appearance,
                    error: result.err().map(|e| format!("Overlay: {e:#}")),
                    _bounds_subscription: bounds_subscription,
                    _feed_subscription: feed_subscription,
                    expiry_task: None,
                    expiry_deadline: None,
                };
                overlay.schedule_expiry(cx);
                overlay
            })
        }) {
            Ok(handle) => {
                let result = handle.update(cx, |overlay, window, _| {
                    if overlay.error.is_some() {
                        window.remove_window();
                    }
                    overlay.error.clone()
                });
                match result {
                    Ok(None) => {
                        self.overlay = Some(handle);
                        self.config.overlay_enabled = true;
                        self.save_pending = true;
                    }
                    Ok(Some(error)) => self.message(error, true),
                    Err(error) => self.message(format!("Overlay setup: {error:#}"), true),
                }
            }
            Err(error) => self.message(format!("Open overlay: {error:#}"), true),
        }
    }

    fn has_pending_events(&self) -> bool {
        !self.rx.is_empty()
            || !self.ui_rx.is_empty()
            || self
                .downloads
                .as_ref()
                .is_some_and(|downloads| downloads.take_changed())
            || (self.save_pending && !self.saving && !self.config_failed)
            || self
                .exit_after
                .is_some_and(|s| self.started.elapsed().as_secs_f64() >= s)
            || self.session.is_some()
    }

    fn session_stopped(&mut self, cx: &mut Context<Self>) {
        let captions = &mut self.captions;
        let mut row = captions.len();
        self.message_scroller.update(cx, |state, cx| {
            captions.retain(|caption| {
                row -= 1;
                if !caption.is_final {
                    state.splice(row..row + 1, 0, cx);
                }
                caption.is_final
            });
        });
        self.session = None;
        self.session_ready = false;
        if self.source.is_none() && self.explicit_log.is_none() && self.replay.is_none() {
            self.restarting = false;
        }
        if self.replay.is_some() && !self.restarting {
            self.desired_running = false;
        }
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(mut workers) = self.shutdown.try_lock() {
            let mut index = 0;
            while index < workers.len() {
                if workers[index].1.is_finished() {
                    let (_, worker) = workers.swap_remove(index);
                    let _ = worker.join();
                } else {
                    index += 1;
                }
            }
        }
        let drops = self.session.as_ref().map(|session| {
            let d = &session.diagnostics;
            [
                &d.audio_packets_dropped,
                &d.hud_observations_dropped,
                &d.chat_messages_dropped,
                &d.status_messages_dropped,
            ]
            .map(|n| n.swap(0, Ordering::Relaxed))
        });
        if let Some(drops) = drops
            && drops.iter().any(|n| *n != 0)
        {
            self.message(format!("Capture overload: dropped audio {}, HUD {}, chat {}, status {}. Speech gaps are kept separate.", drops[0], drops[1], drops[2], drops[3]), true);
        }
        for _ in 0..64 {
            let Ok(event) = self.rx.try_recv() else {
                break;
            };
            match event {
                AppEvent::Caption(caption) => {
                    if self.session.is_some() {
                        let previous_len = self.captions.len();
                        let change = upsert(&mut self.captions, caption);
                        let len = self.captions.len();
                        let overlay_index = match change {
                            CaptionChange::Added => Some(0),
                            CaptionChange::Updated(index) if index < 2 => Some(index),
                            _ => None,
                        };
                        if let Some(caption) =
                            overlay_index.and_then(|index| self.captions.get(index))
                        {
                            self.overlay_feed.update(cx, |messages, cx| {
                                messages.push(caption, Instant::now());
                                cx.notify();
                            });
                        }
                        self.message_scroller.update(cx, |state, cx| match change {
                            CaptionChange::Added => {
                                if previous_len == len {
                                    state.splice(0..1, 0, cx);
                                }
                                state.append(1, cx);
                            }
                            CaptionChange::Updated(index) => {
                                let row = len - 1 - index;
                                state.remeasure_items(row..row + 1, cx);
                            }
                            CaptionChange::Ignored => {}
                        });
                    }
                }
                AppEvent::Status {
                    component,
                    message,
                    is_error,
                } => {
                    if component.eq_ignore_ascii_case("hud") || component == "Replay HUD" {
                        self.speaker_status = Some((message.clone(), is_error));
                    }
                    if is_error {
                        self.message(format!("{component} · {message}"), true);
                    }
                }
                AppEvent::Ready => {
                    self.session_ready = self
                        .session
                        .as_ref()
                        .is_some_and(|session| !session.stop.load(Ordering::Relaxed));
                }
            }
        }
        if self.rx.is_empty()
            && let Some(completion) = self.session.as_ref().and_then(|s| s.take_completion())
        {
            let drops = completion.diagnostics;
            if drops.iter().any(|count| *count != 0) {
                self.message(format!("Capture overload: dropped audio {}, HUD {}, chat {}, status {}. Speech gaps are kept separate.", drops[0], drops[1], drops[2], drops[3]), true);
            }
            if let session::SessionOutcome::Failed(error) = completion.outcome {
                self.desired_running = false;
                self.message(error, true);
            }
            self.session_stopped(cx);
        }
        while let Ok(result) = self.ui_rx.try_recv() {
            match result {
                UiResult::Prepared(result, preferred, voice_enabled) => {
                    self.preparing_assets = false;
                    if voice_enabled != self.config.voice_enabled {
                        continue;
                    }
                    match result {
                        Ok(languages) => {
                            if self.first_run
                                && let Some(code) = preferred
                                && languages.iter().any(|l| l.code == code)
                            {
                                self.config.target_language = code;
                            }
                            if !languages
                                .iter()
                                .any(|l| l.code == self.config.target_language)
                            {
                                self.asset_error = Some(format!(
                                    "Saved output language '{}' is not supported. Choose a language below.",
                                    self.config.target_language
                                ));
                            }
                            let selected = languages
                                .iter()
                                .find(|language| language.code == self.config.target_language)
                                .map(|language| language.code);
                            self.prepare_source_languages(&languages, window, cx);
                            self.language_select.update(cx, |state, cx| {
                                state.set_items(SearchableVec::new(languages), window, cx);
                                if let Some(code) = selected {
                                    state.set_selected_value(&code, window, cx);
                                }
                            });
                            self.assets_ready = true;
                            self.save_pending |= self.first_run;
                            self.first_run = false;
                        }
                        Err(error) => self.asset_error = Some(error),
                    }
                }
                UiResult::Source(result) => match result {
                    Ok(source) => {
                        if self.source != source && self.replay.is_none() {
                            self.restarting =
                                source.is_some() && self.desired_running && self.session.is_some();
                            self.stop();
                        }
                        self.source = source;
                    }
                    Err(error) => self.message(error, true),
                },
                UiResult::Saved(result) => {
                    self.saving = false;
                    if let Err(error) = result {
                        self.message(error, true);
                    }
                }
                UiResult::Recovered(result) => {
                    self.saving = false;
                    match result {
                        Ok(()) => {
                            self.config_failed = false;
                            self.messages.clear();
                        }
                        Err(error) => self.message(error, true),
                    }
                }
            }
        }
        if !self.assets_ready
            && !self.preparing_assets
            && self.asset_error.is_none()
            && self
                .downloads
                .as_ref()
                .is_none_or(|downloads| downloads.all_ready(self.config.voice_enabled))
        {
            self.prepare_assets();
        }
        if self.page == Page::Setup
            && self.assets_ready
            && self.asset_error.is_none()
            && self.config.setup_completed
            && !self.config_failed
        {
            self.page = Page::Home;
        }
        if !self.page.is_setup()
            && self.assets_ready
            && self.config.overlay_enabled
            && self.overlay.is_none()
        {
            self.overlay(cx);
        }
        self.start_if_ready(cx);
        self.save();
        if self
            .exit_after
            .is_some_and(|s| self.started.elapsed().as_secs_f64() >= s)
        {
            self.close(cx);
        }
    }

    fn run_state(&self) -> RunState {
        if !self.desired_running || self.config_failed || self.asset_error.is_some() {
            RunState::Paused
        } else if self.restarting && self.session.is_some() {
            RunState::Restarting
        } else if self.source.is_none() && self.explicit_log.is_none() && self.replay.is_none() {
            RunState::Waiting
        } else if !self.config.voice_enabled
            && self.config.console_log.is_none()
            && self.explicit_log.is_none()
        {
            RunState::WaitingForConsole
        } else if self.session_ready {
            RunState::Live
        } else {
            RunState::Starting
        }
    }

    fn prepare_assets(&mut self) {
        self.preparing_assets = true;
        let config = self.config.clone();
        let tx = self.ui_tx.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || {
            let result = assets::prepare(&config).map_err(|e| format!("{e:#}"));
            let _ = tx.send_timeout(
                UiResult::Prepared(
                    result,
                    cs_platform::preferred_language(),
                    config.voice_enabled,
                ),
                Duration::from_secs(1),
            );
        });
        self.shutdown.lock().unwrap().push((stop, worker));
    }

    fn open_licenses(&mut self, cx: &mut Context<Self>) {
        if self.license_browser.is_some_and(|handle| {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        }) {
            return;
        }
        match licenses::open(self.asset_root.clone(), cx) {
            Ok(handle) => self.license_browser = Some(handle),
            Err(error) => self.message(format!("Open-source licenses: {error:#}"), true),
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.overlay
            && let Ok(bounds) = handle.update(cx, |_, window, _| window.bounds())
        {
            self.config.overlay_x = bounds.origin.x.into();
            self.config.overlay_y = bounds.origin.y.into();
            self.config.overlay_width = bounds.size.width.into();
            self.config.overlay_height = bounds.size.height.into();
        }
        if !self.config_failed {
            *self.final_settings.lock().unwrap() =
                Some((self.config_path.clone(), self.saved_config()));
        }
        self.stop();
        // No UI consumer runs after quit. Disconnect delivery before shutdown joins,
        // including when the bounded event queue is full.
        drop(std::mem::replace(&mut self.rx, crossbeam_channel::never()));
        cx.quit();
    }

    fn saved_config(&self) -> AppConfig {
        let mut config = self.config.clone();
        config.process_id = None;
        config.capture_window = None;
        config.console_log = self.explicit_log.clone();
        if let Some(path) = &mut config.console_log
            && let Ok(relative) = path.strip_prefix(&self.asset_root)
        {
            *path = relative.to_owned();
        }
        for path in [
            &mut config.asr_manifest,
            &mut config.translation_model,
            &mut config.ocr_config,
        ] {
            if let Ok(relative) = path.strip_prefix(&self.asset_root) {
                *path = relative.to_owned();
            }
        }
        config
    }

    fn move_overlay(&mut self, moving: bool, cx: &mut Context<Self>) {
        self.locked = !moving;
        if self.overlay.is_some() {
            self.overlay(cx);
            self.overlay(cx);
        } else if moving {
            self.overlay(cx);
        }
    }

    fn overlay_moved(
        &mut self,
        source: AnyWindowHandle,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.locked
            || self
                .overlay
                .is_none_or(|handle| AnyWindowHandle::from(handle) != source)
        {
            return;
        }
        let next = (
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        );
        let current = (
            self.config.overlay_x,
            self.config.overlay_y,
            self.config.overlay_width,
            self.config.overlay_height,
        );
        if current == next {
            return;
        }
        (
            self.config.overlay_x,
            self.config.overlay_y,
            self.config.overlay_width,
            self.config.overlay_height,
        ) = next;
        cx.notify();
    }

    fn overlay_position(&mut self, horizontal: f32, vertical: f32, cx: &mut Context<Self>) {
        let visible = self.overlay.is_some();
        if visible {
            self.overlay(cx);
        }
        if let Some(display) = cx.primary_display() {
            let bounds = display.bounds();
            self.config.overlay_x = f32::from(bounds.origin.x)
                + 24.
                + (f32::from(bounds.size.width) - self.config.overlay_width - 48.).max(0.)
                    * horizontal;
            self.config.overlay_y = f32::from(bounds.origin.y)
                + 24.
                + (f32::from(bounds.size.height) - self.config.overlay_height - 48.).max(0.)
                    * vertical;
        }
        if visible {
            self.overlay(cx);
        }
        self.save_pending = true;
    }
}

enum CaptionChange {
    Added,
    Updated(usize),
    Ignored,
}

fn upsert(history: &mut VecDeque<Caption>, caption: Caption) -> CaptionChange {
    if let Some((index, old)) = history
        .iter_mut()
        .enumerate()
        .find(|(_, old)| old.id == caption.id)
    {
        if old.is_final && !caption.is_final {
            return CaptionChange::Ignored;
        }
        *old = caption;
        CaptionChange::Updated(index)
    } else {
        history.push_front(caption);
        history.truncate(200);
        CaptionChange::Added
    }
}

fn speaker_text(c: &Caption) -> String {
    if c.speakers.is_empty() {
        if c.channel == "voice" {
            "Voice".into()
        } else {
            "Chat".into()
        }
    } else {
        let mut text = String::new();
        if c.speakers.len() > 1 {
            text.push_str("Candidates · ");
        }
        for (index, speaker) in c.speakers.iter().enumerate() {
            if index != 0 {
                text.push_str(" / ");
            }
            text.push_str(&speaker.text);
            if speaker.truncated && !speaker.text.contains('…') && !speaker.text.contains("...") {
                text.push('…');
            }
        }
        text
    }
}

#[derive(Clone, Copy)]
struct OverlayAppearance {
    show_original: bool,
    show_labels: bool,
    font_size: u8,
    opacity: u8,
}

impl From<&AppConfig> for OverlayAppearance {
    fn from(config: &AppConfig) -> Self {
        Self {
            show_original: config.overlay_show_original,
            show_labels: config.show_message_labels,
            font_size: config.overlay_font_size,
            opacity: config.overlay_opacity,
        }
    }
}

fn caption_display_text(caption: &Caption) -> &String {
    if caption.translated.is_empty() || caption.translation_error.is_some() {
        &caption.original
    } else {
        &caption.translated
    }
}

fn overlay_caption(caption: &Caption, appearance: OverlayAppearance) -> Div {
    let text = caption_display_text(caption);
    let text = if caption.translation_error.is_none()
        && (caption.translation_pending || !caption.is_final)
    {
        gpui_kit::component::shimmer::ShimmerText::new(text.clone())
            .id(("overlay-caption", caption.id))
            .into_any_element()
    } else {
        div().child(text.clone()).into_any_element()
    };
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap(px(2.))
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(rgba(
            0x22242700 | (u32::from(appearance.opacity) * 255 / 100),
        ))
        .when(appearance.show_labels, |card| {
            card.child(
                div()
                    .text_size(px(10.))
                    .line_clamp(1)
                    .text_color(rgb(0xe9c27f))
                    .child(speaker_text(caption)),
            )
        })
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(f32::from(appearance.font_size)))
                .line_clamp(3)
                .text_color(rgb(0xf3f0e9))
                .child(text),
        )
        .when(caption.translation_error.is_some(), |card| {
            card.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(0xe9a07f))
                    .child("Translation failed · choose source language in the caption feed"),
            )
        })
        .when(
            appearance.show_original
                && !caption.translated.is_empty()
                && caption.translation_error.is_none(),
            |card| {
                card.child(
                    div()
                        .text_size(px(10.))
                        .line_clamp(2)
                        .text_color(rgb(0xbab6ab))
                        .child(caption.original.clone()),
                )
            },
        )
}

struct Overlay {
    feed: Entity<OverlayMessages>,
    locked: bool,
    appearance: OverlayAppearance,
    error: Option<String>,
    _bounds_subscription: Option<Subscription>,
    _feed_subscription: Subscription,
    expiry_task: Option<Task<()>>,
    expiry_deadline: Option<Instant>,
}
impl Overlay {
    fn schedule_expiry(&mut self, cx: &mut Context<Self>) {
        let next = self.feed.read(cx).next_expiry();
        if let (Some(current), Some(next)) = (self.expiry_deadline, next)
            && current <= next
        {
            return;
        }
        self.expiry_deadline = next;
        self.expiry_task = next.map(|deadline| {
            cx.spawn(async move |view, cx| {
                cx.background_executor()
                    .timer(deadline.saturating_duration_since(Instant::now()))
                    .await;
                let _ = view.update(cx, |this, cx| {
                    this.expiry_task = None;
                    this.expiry_deadline = None;
                    this.feed.update(cx, |messages, cx| {
                        if messages.expire(Instant::now()) {
                            cx.notify();
                        }
                    });
                    this.schedule_expiry(cx);
                });
            })
        });
    }
}
impl Render for Overlay {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div().size_full().flex().flex_col().justify_end().gap_1();
        if !self.locked {
            root = root
                .border_1()
                .border_color(rgb(0xe6ab45))
                .bg(rgba(0x22242744))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        let result = (|| -> Result<()> {
                            let handle = HasWindowHandle::window_handle(window)
                                .map_err(|error| anyhow::anyhow!("Read overlay handle: {error:?}"))?
                                .as_raw();
                            let RawWindowHandle::Win32(handle) = handle else {
                                anyhow::bail!("Native overlay movement requires Windows");
                            };
                            cs_platform::begin_overlay_move(handle.hwnd.get())
                        })();
                        if let Err(error) = result {
                            this.error = Some(format!("Move overlay: {error:#}"));
                            cx.notify();
                        }
                    }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xe6ab45))
                        .child("Drag to position · Choose Done moving in Settings"),
                );
        }
        if let Some(error) = &self.error {
            root = root.child(div().p_2().bg(rgb(0x842f3f)).child(error.clone()));
        }
        for caption in self.feed.read(cx).captions(Instant::now()) {
            root = root.child(overlay_caption(caption, self.appearance));
        }
        root
    }
}

fn place_overlay(config: &mut AppConfig, bounds: Bounds<Pixels>) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    config.overlay_width = 560f32.min((width - 40.).max(280.));
    config.overlay_height = 150f32.min((height - 80.).max(100.));
    config.overlay_x = f32::from(bounds.origin.x) + (width - config.overlay_width) / 2.;
    config.overlay_y = f32::from(bounds.origin.y) + height - config.overlay_height - 60.;
}

fn main() {
    let args = Args::parse();
    if args.build_info {
        print!(
            "{}",
            include_str!(concat!(env!("OUT_DIR"), "/build-info.json"))
        );
        return;
    }
    if args.session_worker {
        if let Err(error) = worker::run_child() {
            eprintln!("Session worker: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    let explicit = args.config.is_some();
    let path = args.config.unwrap_or_else(cs_core::settings_path);
    let first_run = !path.exists();
    let asset_root = match std::env::current_exe() {
        Ok(exe) => assets::root_for_executable(&exe),
        Err(error) => {
            eprintln!("Determine application location: {error}");
            std::process::exit(1);
        }
    };
    let (mut config, error) = if path.exists() {
        match AppConfig::load(&path) {
            Ok(config) => (config, None),
            Err(error) => (
                AppConfig::default(),
                Some(format!("Saved preferences could not be read: {error:#}")),
            ),
        }
    } else {
        (AppConfig::default(), None)
    };
    config.process_id = None;
    config.capture_window = None;
    if !explicit {
        let defaults = AppConfig::default();
        config.asr_manifest = defaults.asr_manifest;
        config.translation_model = defaults.translation_model;
        config.ocr_config = defaults.ocr_config;
        config.console_log = None;
    }
    assets::resolve(&mut config, &asset_root);
    let shutdown: session::ShutdownWorkers = Arc::new(Mutex::new(Vec::new()));
    let app_shutdown = shutdown.clone();
    let final_settings = Arc::new(Mutex::new(None::<(PathBuf, AppConfig)>));
    let app_final_settings = final_settings.clone();
    gpui_kit::application()
        .with_assets(ui_assets::UiAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            Theme::update(cx, |theme| {
                theme.background = rgb(0x222427).into();
                theme.foreground = rgb(0xf1eee7).into();
                theme.primary = rgb(0xe6ab45).into();
                theme.primary_hover = rgb(0xf4bd60).into();
                theme.primary_active = rgb(0xc69137).into();
                theme.primary_foreground = rgb(0x101010).into();
                theme.button_primary = rgb(0xe6ab45).into();
                theme.button_primary_hover = rgb(0xf4bd60).into();
                theme.button_primary_active = rgb(0xc69137).into();
                theme.button_primary_foreground = rgb(0x101010).into();
                theme.ring = rgb(0xe6ab45).into();
                theme.accent = rgb(0x494234).into();
                theme.accent_foreground = rgb(0xf1eee7).into();
                theme.secondary = rgb(0x343638).into();
                theme.secondary_foreground = rgb(0xf1eee7).into();
                theme.button = rgb(0x343638).into();
                theme.button_hover = rgb(0x4a463d).into();
                theme.button_foreground = rgb(0xf1eee7).into();
                theme.border = rgb(0x4b4c47).into();
                theme.muted_foreground = rgb(0xbab6ab).into();
                theme.title_bar = rgb(0x292b2e).into();
                theme.title_bar_border = rgb(0x4b4c47).into();
                theme.sidebar = rgb(0x292b2e).into();
                theme.sidebar_foreground = rgb(0xf1eee7).into();
                theme.sidebar_accent = rgb(0x494234).into();
                theme.sidebar_accent_foreground = rgb(0xf1eee7).into();
                theme.sidebar_border = rgb(0x4b4c47).into();
                theme.popover = rgb(0x292b2e).into();
                theme.popover_foreground = rgb(0xf1eee7).into();
                theme.progress_bar = rgb(0xe6ab45).into();
            });
            let display_bounds = cx
                .primary_display()
                .map(|display| display.bounds())
                .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1200.), px(920.))));
            let on_display = cx.displays().iter().any(|display| {
                let b = display.bounds();
                config.overlay_x >= f32::from(b.origin.x)
                    && config.overlay_y >= f32::from(b.origin.y)
                    && config.overlay_x + config.overlay_width
                        <= f32::from(b.origin.x + b.size.width)
                    && config.overlay_y + config.overlay_height
                        <= f32::from(b.origin.y + b.size.height)
            });
            if first_run || !on_display {
                place_overlay(&mut config, display_bounds);
            }
            let initial_size = pages::page_size(
                if explicit { Page::Home } else { Page::Setup },
                display_bounds.size,
            );
            let result = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        initial_size,
                        cx,
                    ))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("CS Translator".into()),
                        ..TitleBar::title_bar_options()
                    }),
                    window_min_size: Some(size(px(660.), px(600.) + TITLEBAR_HEIGHT)),
                    ..TitleBar::window_options()
                },
                cx,
                move |window, cx| {
                    let view = cx.new(|cx| {
                        let (tx, rx) = bounded(128);
                        let (ui_tx, ui_rx) = bounded(32);
                        let downloads = (!explicit).then(|| {
                            downloads::Manager::new(
                                asset_root.clone(),
                                config.voice_enabled,
                                &app_shutdown,
                            )
                        });
                        let language_select = cx.new(|cx| {
                            SelectState::new(
                                SearchableVec::new(Vec::<Language>::new()),
                                None,
                                window,
                                cx,
                            )
                            .searchable(true)
                        });
                        let language_subscription = cx.subscribe_in(
                            &language_select,
                            window,
                            |this: &mut Desktop, _, event, _, cx| {
                                if let SelectEvent::Confirm(Some(code)) = event {
                                    this.asset_error = None;
                                    this.language(code, cx);
                                    this.save();
                                    cx.notify();
                                }
                            },
                        );
                        let source_select = cx.new(|cx| {
                            SelectState::new(
                                SearchableVec::new(Vec::<Language>::new()),
                                None,
                                window,
                                cx,
                            )
                            .searchable(true)
                        });
                        let player_select = cx.new(|cx| {
                            SelectState::new(
                                SearchableVec::new(Vec::<Language>::new()),
                                None,
                                window,
                                cx,
                            )
                            .searchable(true)
                        });
                        let source_subscription = cx.subscribe_in(
                            &source_select,
                            window,
                            |this: &mut Desktop, _, event, _, cx| {
                                if let SelectEvent::Confirm(Some(code)) = event
                                    && this.config.source_language != *code
                                {
                                    this.config.source_language = (*code).into();
                                    this.translation_policy_changed(cx);
                                }
                            },
                        );
                        let player_subscription = cx.subscribe_in(
                            &player_select,
                            window,
                            |this: &mut Desktop, _, event, window, cx| {
                                if this.language_editor_valid
                                    && let SelectEvent::Confirm(Some(code)) = event
                                {
                                    this.set_player_language(code, cx);
                                    if this.language_editor_player.is_none() {
                                        this.source_select.update(cx, |state, cx| {
                                            state.set_selected_value(code, window, cx);
                                        });
                                    }
                                }
                            },
                        );
                        let overlay_feed = cx.new(|_| {
                            OverlayMessages::new(Duration::from_secs(u64::from(
                                config.overlay_message_expiry_secs,
                            )))
                        });
                        let mut app = Desktop {
                            explicit_log: config.console_log.clone(),
                            config,
                            config_path: path,
                            asset_root,
                            replay: args.replay,
                            source: None,
                            language_select,
                            _language_subscription: language_subscription,
                            source_select,
                            player_select,
                            language_editor_player: None,
                            player_languages: Vec::new(),
                            language_editor_valid: false,
                            source_languages: Vec::new(),
                            _source_subscription: source_subscription,
                            _player_subscription: player_subscription,
                            message_scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
                            page: if explicit { Page::Home } else { Page::Setup },
                            fitted_page: None,
                            keyboard_focus: cx.focus_handle(),
                            settings_pages: std::cell::OnceCell::new(),
                            downloads,
                            speaker_status: None,
                            managed_models: !explicit,
                            preparing_assets: false,
                            first_run,
                            assets_ready: false,
                            session_ready: false,
                            asset_error: None,
                            config_failed: error.is_some(),
                            desired_running: !args.paused,
                            restarting: false,
                            save_pending: first_run,
                            saving: false,
                            final_settings: app_final_settings.clone(),
                            session: None,
                            tx,
                            rx,
                            ui_tx,
                            ui_rx,
                            captions: VecDeque::new(),
                            overlay_feed,
                            overlay: None,
                            license_browser: None,
                            locked: true,
                            messages: VecDeque::new(),
                            started: Instant::now(),
                            exit_after: args.exit_after,
                            shutdown: app_shutdown.clone(),
                        };
                        if let Some(error) = error {
                            app.message(error, true);
                        }
                        if app.replay.is_none() {
                            observe(app.ui_tx.clone(), &app.shutdown);
                        }
                        if explicit {
                            app.prepare_assets();
                        }
                        cx.spawn_in(window, async move |view, cx| {
                            let mut cadence = Duration::from_millis(25);
                            loop {
                                cx.background_executor().timer(cadence).await;
                                if view
                                    .update_in(cx, |this: &mut Desktop, window, cx| {
                                        if this.has_pending_events() {
                                            this.poll(window, cx);
                                            cx.notify();
                                        }
                                        cadence = if this.session.is_some() || this.preparing_assets
                                        {
                                            Duration::from_millis(25)
                                        } else {
                                            Duration::from_millis(250)
                                        };
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        })
                        .detach();
                        app
                    });
                    let closing_view = view.downgrade();
                    window.on_window_should_close(cx, move |_, cx| {
                        let _ = closing_view.update(cx, |this, cx| this.close(cx));
                        true
                    });
                    view
                },
            );
            if let Err(error) = result {
                eprintln!("Open desktop: {error:#}");
                cx.quit();
            }
            cx.activate(true);
        });
    let workers: Vec<_> = shutdown.lock().unwrap().drain(..).collect();
    for (stop, _) in &workers {
        stop.store(true, Ordering::Relaxed);
    }
    for (_, worker) in workers {
        let _ = worker.join();
    }
    if let Some((path, config)) = final_settings.lock().unwrap().take()
        && let Err(error) = config.save(&path)
    {
        eprintln!("Save preferences on exit: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    fn caption(id: u64, text: &str, is_final: bool) -> Caption {
        Caption {
            id,
            start_ms: 0,
            end_ms: 1,
            channel: "voice".into(),
            speakers: vec![],
            original: text.into(),
            translated: text.into(),
            translation_pending: false,
            translation_error: None,
            language: None,
            is_final,
        }
    }
    #[test]
    fn failed_caption_preserves_real_text_until_translation_recovers() {
        let mut caption = caption(7, "bonjour", true);
        caption.translated = "hello".into();
        assert_eq!(caption_display_text(&caption), "hello");
        caption.translation_error =
            Some("Cannot identify source language; select it explicitly".into());
        assert_eq!(caption_display_text(&caption), "bonjour");
        caption.translation_error = None;
        caption.translated = "good morning".into();
        assert_eq!(caption_display_text(&caption), "good morning");
    }
    #[test]
    fn final_caption_cannot_regress_to_partial() {
        let mut history = VecDeque::new();
        upsert(&mut history, caption(1, "partial", false));
        upsert(&mut history, caption(1, "final", true));
        upsert(&mut history, caption(1, "late partial", false));
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].original, "final");
    }
    #[test]
    fn history_is_bounded() {
        let mut history = VecDeque::new();
        for id in 0..300 {
            upsert(&mut history, caption(id, "text", true));
        }
        assert_eq!(history.len(), 200);
        assert_eq!(history[0].id, 299);
    }
    #[test]
    fn speaker_candidates_preserve_unicode_and_truncation() {
        let mut caption = caption(1, "text", true);
        let mut truncated = cs_core::SpeakerLabel::new("队友");
        truncated.truncated = true;
        let mut already_marked = cs_core::SpeakerLabel::new("Al…");
        already_marked.truncated = true;
        caption.speakers = vec![truncated, already_marked];
        assert_eq!(speaker_text(&caption), "Candidates · 队友… / Al…");
    }
}
