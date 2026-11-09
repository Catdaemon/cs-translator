use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::{Sender, bounded};
use cs_core::{
    AppConfig, AppEvent, Caption, CaptureDiagnostics, CaptureEvent, HudObservation, Recognition,
    TranslationCadence, speakers_for_interval,
};
use cs_inference::SpeechRecognizer;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
#[path = "translation_worker.rs"]
mod translation_worker;
use translation_worker::{Job, TranslationWorker};

#[derive(Debug)]
pub enum SessionOutcome {
    Completed,
    Cancelled,
    Failed(String),
}
#[derive(Debug)]
pub struct SessionCompletion {
    pub outcome: SessionOutcome,
    pub diagnostics: [u64; 4],
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TranslationPolicy {
    pub target: String,
    pub source: String,
    pub player_languages: Vec<cs_core::PlayerLanguage>,
    pub interval_ms: u64,
}
pub struct Session {
    pub stop: Arc<AtomicBool>,
    pub diagnostics: Arc<CaptureDiagnostics>,
    completion: Arc<Mutex<Option<SessionCompletion>>>,
    policy: Arc<Mutex<Option<TranslationPolicy>>>,
}
impl Session {
    pub fn take_completion(&self) -> Option<SessionCompletion> {
        self.completion.lock().unwrap().take()
    }
    pub fn update_translation(
        &self,
        config: &AppConfig,
        tags: &[cs_core::PlayerLanguage],
    ) -> Result<()> {
        config.validate()?;
        ensure!(tags.len() <= 256, "Too many session language corrections");
        for (index, tag) in tags.iter().enumerate() {
            ensure!(
                !tag.player.text.trim().is_empty(),
                "Player label must not be empty"
            );
            ensure!(
                !tags[..index].iter().any(|other| other.player == tag.player),
                "Duplicate session player label"
            );
        }
        let source = if config.source_language.trim().eq_ignore_ascii_case("auto") {
            "auto".into()
        } else {
            cs_core::normalize_language(&config.source_language)?
        };
        let player_languages = tags
            .iter()
            .filter(|tag| !tag.language.trim().eq_ignore_ascii_case("auto"))
            .map(|tag| {
                Ok(cs_core::PlayerLanguage {
                    player: tag.player.clone(),
                    language: cs_core::normalize_language(&tag.language)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        *self.policy.lock().unwrap() = Some(TranslationPolicy {
            target: cs_core::normalize_language(&config.target_language)?,
            source,
            player_languages,
            interval_ms: config.partial_translation_interval_ms,
        });
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
pub type ShutdownWorkers = Arc<Mutex<Vec<(Arc<AtomicBool>, thread::JoinHandle<()>)>>>;
#[cfg(test)]
pub(crate) fn diagnostic_counts(d: &CaptureDiagnostics) -> [u64; 4] {
    [
        &d.audio_packets_dropped,
        &d.hud_observations_dropped,
        &d.chat_messages_dropped,
        &d.status_messages_dropped,
    ]
    .map(|c| c.load(Ordering::Relaxed))
}
pub fn start(
    config: AppConfig,
    replay: Option<PathBuf>,
    tx: Sender<AppEvent>,
    shutdown: ShutdownWorkers,
) -> Session {
    let stop = Arc::new(AtomicBool::new(false));
    let diagnostics = Arc::new(CaptureDiagnostics::default());
    let completion = Arc::new(Mutex::new(None));
    let policy = Arc::new(Mutex::new(None));
    let (s, d, c, p) = (
        stop.clone(),
        diagnostics.clone(),
        completion.clone(),
        policy.clone(),
    );
    let worker = thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::worker::run_parent(config, replay, &tx, s.clone(), d.clone(), p)
        }));
        let outcome = match result {
            Ok(Err(error)) => SessionOutcome::Failed(format!("{error:#}")),
            Err(_) => SessionOutcome::Failed("Session coordinator panicked".into()),
            Ok(Ok(())) if s.load(Ordering::Relaxed) => SessionOutcome::Cancelled,
            Ok(Ok(())) => SessionOutcome::Completed,
        };
        let final_counts = [
            &d.audio_packets_dropped,
            &d.hud_observations_dropped,
            &d.chat_messages_dropped,
            &d.status_messages_dropped,
        ]
        .map(|counter| counter.swap(0, Ordering::Relaxed));
        *c.lock().unwrap() = Some(SessionCompletion {
            outcome,
            diagnostics: final_counts,
        });
    });
    shutdown.lock().unwrap().push((stop.clone(), worker));
    Session {
        stop,
        diagnostics,
        completion,
        policy,
    }
}
fn status(
    tx: &Sender<AppEvent>,
    diagnostics: &CaptureDiagnostics,
    component: &str,
    message: impl Into<String>,
    is_error: bool,
) {
    if tx
        .try_send(AppEvent::Status {
            component: component.into(),
            message: message.into(),
            is_error,
        })
        .is_err()
    {
        diagnostics
            .status_messages_dropped
            .fetch_add(1, Ordering::Relaxed);
    }
}
fn deliver(tx: &Sender<AppEvent>, mut event: AppEvent, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match tx.send_timeout(event, Duration::from_millis(25)) {
            Ok(()) => return,
            Err(crossbeam_channel::SendTimeoutError::Timeout(value)) => event = value,
            Err(_) => return,
        }
    }
}
struct Pending {
    caption: Caption,
    source: String,
    submitted: Option<(String, String, u64, bool)>,
}
impl Pending {
    fn resolved(&mut self, result: Result<String>, final_result: bool) {
        match result {
            Ok(text) => {
                self.caption.translated = text;
                self.caption.translation_error = None;
                self.caption.translation_pending = false;
            }
            Err(error) => {
                self.submitted = None;
                self.caption.translation_error = Some(format!("{error:#}"));
                self.caption.translated.clear();
                if final_result {
                    self.caption.translation_pending = false;
                }
            }
        }
    }
}
struct Pipeline {
    recognizer: Option<SpeechRecognizer>,
    translator: TranslationWorker,
    config: AppConfig,
    player_languages: Vec<cs_core::PlayerLanguage>,
    diagnostics: Arc<CaptureDiagnostics>,
    hud: VecDeque<HudObservation>,
    captions: HashMap<u64, Pending>,
    recent: VecDeque<u64>,
    next_id: u64,
    active: Option<u64>,
    cadence: TranslationCadence,
    clock: Instant,
    policy: u64,
}
impl Pipeline {
    fn publish(&self, id: u64, tx: &Sender<AppEvent>, stop: &AtomicBool) {
        if let Some(pending) = self.captions.get(&id) {
            deliver(tx, AppEvent::Caption(pending.caption.clone()), stop);
        }
    }
    fn insert(&mut self, caption: Caption, source: String) -> Result<()> {
        // Pending finals are never silently evicted. Fail explicitly under sustained overload.
        while self.captions.len() >= 512 {
            let Some(index) = self.recent.iter().position(|id| {
                self.captions
                    .get(id)
                    .is_some_and(|p| !p.caption.translation_pending && p.caption.is_final)
            }) else {
                bail!("Translation backlog exhausted");
            };
            let id = self.recent.remove(index).unwrap();
            self.captions.remove(&id);
        }
        self.recent.push_back(caption.id);
        self.captions.insert(
            caption.id,
            Pending {
                caption,
                source,
                submitted: None,
            },
        );
        Ok(())
    }
    fn recognition(
        &mut self,
        r: Recognition,
        tx: &Sender<AppEvent>,
        stop: &AtomicBool,
    ) -> Result<()> {
        if r.text.trim().is_empty() {
            return Ok(());
        }
        let speakers = speakers_for_interval(self.hud.make_contiguous(), r.start_ms, r.end_ms);
        let source = self
            .config
            .translation_source(&self.player_languages, &speakers, r.language.as_deref())
            .to_owned();
        let id = if let Some(id) = self.active {
            id
        } else {
            let id = self.next_id;
            self.next_id += 1;
            self.insert(
                Caption {
                    id,
                    start_ms: r.start_ms,
                    end_ms: r.end_ms,
                    channel: "voice".into(),
                    speakers: vec![],
                    original: String::new(),
                    translated: String::new(),
                    translation_pending: true,
                    translation_error: None,
                    language: r.language.clone(),
                    is_final: false,
                },
                source.clone(),
            )?;
            self.active = Some(id);
            id
        };
        let pending = self.captions.get_mut(&id).unwrap();
        if pending.caption.original != r.text || pending.source != source {
            self.policy += 1;
            pending.caption.translation_pending = true;
            pending.caption.translation_error = None;
            pending.caption.translated.clear();
            pending.submitted = None;
        }
        pending.source = source;
        pending.caption.original = r.text;
        pending.caption.start_ms = r.start_ms;
        pending.caption.end_ms = r.end_ms;
        pending.caption.language = r.language;
        pending.caption.is_final = r.is_final;
        pending.caption.speakers = speakers;
        self.publish(id, tx, stop);
        if r.is_final
            || self
                .cadence
                .due(self.clock.elapsed().as_millis() as u64, false)
        {
            if r.is_final {
                self.pump_finals()?;
            } else {
                self.submit(id)?;
            }
        }
        if r.is_final {
            self.active = None;
            self.cadence.reset();
        }
        Ok(())
    }
    fn pump_finals(&mut self) -> Result<()> {
        for index in 0..self.recent.len() {
            let id = self.recent[index];
            if self.captions.get(&id).is_some_and(|p| p.caption.is_final) && !self.submit(id)? {
                break;
            }
        }
        Ok(())
    }
    fn submit(&mut self, id: u64) -> Result<bool> {
        let pending = self.captions.get_mut(&id).unwrap();
        if !pending.caption.translation_pending {
            return Ok(true);
        }
        if pending
            .submitted
            .as_ref()
            .is_some_and(|(text, source, _policy, final_result)| {
                text == &pending.caption.original
                    && source == &pending.source
                    && *final_result == pending.caption.is_final
            })
        {
            return Ok(true);
        }
        let key = (
            pending.caption.original.clone(),
            pending.source.clone(),
            self.policy,
            pending.caption.is_final,
        );
        let submission = self.translator.submit(Job {
            id,
            text: key.0.clone(),
            source: key.1.clone(),
            target: self.config.target_language.clone(),
            policy: self.policy,
            final_result: key.3,
        })?;
        if submission.accepted {
            pending.submitted = Some(key);
            if self.active == Some(id) && !pending.caption.is_final {
                self.cadence
                    .started(self.clock.elapsed().as_millis() as u64);
            }
        }
        if let Some(evicted) = submission.evicted
            && let Some(pending) = self.captions.get_mut(&evicted.id)
            && pending
                .submitted
                .as_ref()
                .is_some_and(|(text, source, policy, final_result)| {
                    text == &evicted.text
                        && source == &evicted.source
                        && *policy == evicted.policy
                        && *final_result == evicted.final_result
                })
        {
            pending.submitted = None;
        }
        Ok(submission.accepted)
    }
    fn translations(&mut self, tx: &Sender<AppEvent>, stop: &AtomicBool) -> Result<()> {
        ensure!(
            !self.translator.is_finished() || stop.load(Ordering::Relaxed),
            "Translation worker stopped unexpectedly"
        );
        while let Ok(result) = self.translator.results.try_recv() {
            let Some(pending) = self.captions.get_mut(&result.job.id) else {
                continue;
            };
            if !matches_revision(
                &pending.caption.original,
                &pending.source,
                pending.submitted.as_ref().map_or(u64::MAX, |key| key.2),
                &result.job,
            ) {
                continue;
            }
            // An in-flight partial cannot supersede an already-submitted final retry.
            if pending.caption.is_final && !result.job.final_result {
                continue;
            }
            pending.resolved(result.result, result.job.final_result);
            self.publish(result.job.id, tx, stop);
        }
        self.pump_finals()?;
        if let Some(id) = self.active
            && self
                .cadence
                .due(self.clock.elapsed().as_millis() as u64, false)
        {
            self.submit(id)?;
        }
        Ok(())
    }
    fn update_policy(
        &mut self,
        policy: TranslationPolicy,
        tx: &Sender<AppEvent>,
        stop: &AtomicBool,
    ) -> Result<()> {
        self.policy += 1;
        let target_changed = self.config.target_language != policy.target;
        self.config.target_language = policy.target;
        self.config.source_language = policy.source;
        self.player_languages = policy.player_languages;
        self.config.partial_translation_interval_ms = policy.interval_ms;
        self.cadence = TranslationCadence::new(policy.interval_ms);
        let mut ids = Vec::new();
        for (id, pending) in &mut self.captions {
            let source = self.config.translation_source(
                &self.player_languages,
                &pending.caption.speakers,
                pending.caption.language.as_deref(),
            );
            let retry_target = target_changed
                && (pending.caption.translation_pending
                    || pending.caption.translation_error.is_some()
                    || Some(*id) == self.active);
            if retry_target || source != pending.source {
                pending.source = source.to_owned();
                pending.caption.translation_pending = true;
                pending.caption.translation_error = None;
                pending.caption.translated.clear();
                pending.submitted = None;
                ids.push(*id);
            }
        }
        for id in ids {
            self.publish(id, tx, stop);
        }
        self.pump_finals()?;
        Ok(())
    }
    fn event(
        &mut self,
        event: CaptureEvent,
        tx: &Sender<AppEvent>,
        stop: &AtomicBool,
    ) -> Result<()> {
        match event {
            CaptureEvent::Audio(chunk) => {
                let Some(recognizer) = self.recognizer.as_mut() else {
                    return Ok(());
                };
                if chunk.discontinuity {
                    status(
                        tx,
                        &self.diagnostics,
                        "Audio",
                        "Audio discontinuity: recognition context reset; speech may be missing",
                        true,
                    );
                }
                for r in recognizer.push(&chunk)? {
                    self.recognition(r, tx, stop)?;
                }
            }
            CaptureEvent::Hud(observation) if self.config.voice_enabled => {
                self.hud.push_back(observation);
                while self.hud.len() > 240 {
                    self.hud.pop_front();
                }
                let observations = self.hud.make_contiguous();
                for pending in self.captions.values_mut() {
                    if pending.caption.channel != "voice" {
                        continue;
                    }
                    let speakers = speakers_for_interval(
                        observations,
                        pending.caption.start_ms,
                        pending.caption.end_ms,
                    );
                    if !speakers.is_empty() && speakers != pending.caption.speakers {
                        pending.caption.speakers = speakers;
                        let source = self.config.translation_source(
                            &self.player_languages,
                            &pending.caption.speakers,
                            pending.caption.language.as_deref(),
                        );
                        if source != pending.source {
                            self.policy += 1;
                            pending.source = source.to_owned();
                            pending.caption.translation_pending = true;
                            pending.caption.translation_error = None;
                            pending.caption.translated.clear();
                            pending.submitted = None;
                        }
                        deliver(tx, AppEvent::Caption(pending.caption.clone()), stop);
                    }
                }
                self.pump_finals()?;
            }
            CaptureEvent::Hud(_) => {}
            CaptureEvent::Chat(chat) => {
                let id = self.next_id;
                self.next_id += 1;
                let speakers = vec![cs_core::SpeakerLabel::new(chat.player)];
                let source = self
                    .config
                    .translation_source(&self.player_languages, &speakers, None)
                    .to_owned();
                self.insert(
                    Caption {
                        id,
                        start_ms: chat.timestamp_ms,
                        end_ms: chat.timestamp_ms,
                        channel: chat.channel,
                        speakers,
                        original: chat.text,
                        translated: String::new(),
                        translation_pending: true,
                        translation_error: None,
                        language: None,
                        is_final: true,
                    },
                    source,
                )?;
                self.publish(id, tx, stop);
                self.pump_finals()?;
            }
            CaptureEvent::Status {
                component,
                message,
                is_error,
            } => status(tx, &self.diagnostics, &component, message, is_error),
        }
        Ok(())
    }
}
fn matches_revision(text: &str, source: &str, policy: u64, job: &Job) -> bool {
    text == job.text && source == job.source && policy == job.policy
}

struct Source {
    name: &'static str,
    essential: bool,
    worker: Option<cs_platform::CaptureWorker>,
}
struct Sources {
    stop: Arc<AtomicBool>,
    workers: Vec<Source>,
}
impl Sources {
    fn add(
        &mut self,
        name: &'static str,
        essential: bool,
        worker: cs_platform::CaptureWorker,
        tx: &Sender<AppEvent>,
        d: &CaptureDiagnostics,
    ) -> Result<()> {
        let ready = if essential {
            worker.wait_ready()
        } else {
            Ok(())
        };
        self.workers.push(Source {
            name,
            essential,
            worker: Some(worker),
        });
        match ready {
            Ok(()) => Ok(()),
            Err(error) if essential => Err(error).with_context(|| format!("Initialize {name}")),
            Err(error) => {
                status(tx, d, name, format!("{error:#}"), true);
                Ok(())
            }
        }
    }
    fn check(
        &mut self,
        replay: bool,
        tx: &Sender<AppEvent>,
        d: &CaptureDiagnostics,
    ) -> Result<bool> {
        let mut audio_done = false;
        for source in &mut self.workers {
            if source.worker.as_ref().is_some_and(|w| w.is_finished()) {
                let result = source.worker.take().unwrap().join();
                if source.essential {
                    result.with_context(|| format!("{} capture failed", source.name))?;
                    ensure!(
                        replay || self.stop.load(Ordering::Relaxed),
                        "{} capture stopped unexpectedly",
                        source.name
                    );
                    audio_done = true;
                } else if let Err(error) = result {
                    status(tx, d, source.name, format!("{error:#}"), true);
                }
            }
        }
        Ok(audio_done)
    }
}
impl Drop for Sources {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for source in &mut self.workers {
            if let Some(worker) = source.worker.take() {
                let _ = worker.join();
            }
        }
    }
}
pub(crate) fn run_local(
    config: AppConfig,
    replay: Option<PathBuf>,
    tx: &Sender<AppEvent>,
    stop: Arc<AtomicBool>,
    diagnostics: Arc<CaptureDiagnostics>,
    policy: Arc<Mutex<Option<TranslationPolicy>>>,
) -> Result<()> {
    config.validate()?;
    ensure!(
        config.voice_enabled || config.console_log.is_some(),
        "Voice translation is off; select a CS2 console log for chat translation"
    );
    // A diagnostic media path is not a text source and must never be decoded in chat-only mode.
    let replay = replay.filter(|_| config.voice_enabled);
    if replay.is_none() && config.process_id.is_some() {
        ensure!(
            cs_platform::list_windows()?
                .iter()
                .any(|w| Some(w.process_id) == config.process_id
                    && Some(w.handle) == config.capture_window),
            "CS2 restarted before capture was ready; pause and resume to reconnect"
        );
    }
    status(
        tx,
        &diagnostics,
        "Models",
        if config.voice_enabled {
            "Loading local speech and translation models…"
        } else {
            "Loading local translation model…"
        },
        false,
    );
    let recognizer = if config.voice_enabled {
        Some(
            SpeechRecognizer::load(
                &config.asr_manifest,
                config.threads,
                &config.source_language,
            )
            .context("Load installed speech recognition model")?,
        )
    } else {
        None
    };
    let translator = TranslationWorker::start(&config, stop.clone())?;
    let mut pipeline = Pipeline {
        recognizer,
        translator,
        config: config.clone(),
        player_languages: Vec::new(),
        diagnostics: diagnostics.clone(),
        hud: VecDeque::new(),
        captions: HashMap::new(),
        recent: VecDeque::new(),
        next_id: 1,
        active: None,
        cadence: TranslationCadence::new(config.partial_translation_interval_ms),
        clock: Instant::now(),
        policy: 0,
    };
    let (capture_tx, capture_rx) = bounded(if replay.is_some() { 16 } else { 128 });
    let (hud_tx, hud_rx) = bounded(if config.voice_enabled && config.hud_enabled {
        240
    } else {
        0
    });
    let mut sources = Sources {
        stop: stop.clone(),
        workers: vec![],
    };
    let is_replay = replay.is_some();
    if let Some(path) = replay {
        if config.hud_enabled
            && path
                .extension()
                .is_some_and(|e| !e.eq_ignore_ascii_case("wav"))
        {
            let (video, cfg, output, stopped) =
                (path.clone(), config.clone(), hud_tx.clone(), stop.clone());
            sources.add(
                "Replay HUD",
                false,
                cs_platform::CaptureWorker::spawn(move |ready| {
                    let mut ocr = cs_platform::HudOcr::load(&cfg.ocr_config)?;
                    ready.send(Ok(())).ok();
                    replay_hud(&video, &output, &stopped, &mut ocr)
                })?,
                tx,
                &diagnostics,
            )?;
        }
        let (output, stopped) = (capture_tx.clone(), stop.clone());
        sources.add(
            "Replay audio",
            true,
            cs_platform::CaptureWorker::spawn(move |ready| {
                replay_media(&path, &output, &stopped, ready)
            })?,
            tx,
            &diagnostics,
        )?;
    } else {
        ensure!(
            config.process_id.is_some() || config.console_log.is_some(),
            "Waiting for a current CS2 process or diagnostic console log"
        );
        if config.voice_enabled
            && let Some(pid) = config.process_id
        {
            sources.add(
                "Audio",
                true,
                cs_platform::spawn_audio(
                    pid,
                    capture_tx.clone(),
                    stop.clone(),
                    diagnostics.clone(),
                )?,
                tx,
                &diagnostics,
            )?;
        }
        if let Some(path) = config.console_log.clone() {
            sources.add(
                "Console",
                !config.voice_enabled || config.process_id.is_none(),
                cs_platform::spawn_log_tail(
                    path,
                    capture_tx.clone(),
                    stop.clone(),
                    diagnostics.clone(),
                )?,
                tx,
                &diagnostics,
            )?;
        }
        if config.voice_enabled
            && config.hud_enabled
            && let Some(window) = config.capture_window
        {
            match cs_platform::spawn_hud_capture(
                window,
                config.ocr_config.clone(),
                hud_tx.clone(),
                stop.clone(),
                diagnostics.clone(),
            ) {
                Ok(worker) => sources.add("HUD", false, worker, tx, &diagnostics)?,
                Err(error) => status(tx, &diagnostics, "HUD", format!("{error:#}"), true),
            }
        }
    }
    drop(capture_tx);
    drop(hud_tx);
    let mut audio_done = sources.check(is_replay, tx, &diagnostics)?;
    deliver(tx, AppEvent::Ready, &stop);
    while !stop.load(Ordering::Relaxed) {
        if let Some(update) = policy.lock().unwrap().take() {
            pipeline.update_policy(update, tx, &stop)?;
        }
        for event in hud_rx.try_iter().take(240) {
            pipeline.event(event, tx, &stop)?;
        }
        match capture_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(event) => pipeline.event(event, tx, &stop)?,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => audio_done = true,
            Err(_) => {}
        }
        audio_done |= sources.check(is_replay, tx, &diagnostics)?;
        pipeline.translations(tx, &stop)?;
        if audio_done && capture_rx.is_empty() {
            // On replay EOF, HUD must finish and its final observations precede ASR flush.
            if is_replay
                && (sources.workers.iter().any(|s| s.worker.is_some()) || !hud_rx.is_empty())
            {
                continue;
            }
            break;
        }
    }
    if !stop.load(Ordering::Relaxed) {
        if let Some(recognizer) = pipeline.recognizer.as_mut() {
            for recognition in recognizer.finish()? {
                pipeline.recognition(recognition, tx, &stop)?;
            }
        }
        while pipeline
            .captions
            .values()
            .any(|p| p.caption.translation_pending)
            && !stop.load(Ordering::Relaxed)
        {
            pipeline.translations(tx, &stop)?;
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}
fn ffmpeg() -> PathBuf {
    std::env::var_os("CS_TRANSLATOR_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .map(|exe| crate::assets::root_for_executable(&exe).join(".tools/ffmpeg.exe"))
                .ok()
                .filter(|p| p.is_file())
                .unwrap_or_else(|| PathBuf::from("ffmpeg"))
        })
}
fn replay_media(
    path: &Path,
    tx: &Sender<CaptureEvent>,
    stop: &Arc<AtomicBool>,
    ready: Sender<std::result::Result<(), String>>,
) -> Result<()> {
    use cs_platform::media::*;
    let child = pcm_command(&ffmpeg(), path, MediaClip::new(0.0, None)?, true)
        .spawn()
        .context("Start FFmpeg replay audio")?;
    let mut child = CancellableChild::new(child, stop);
    let mut reader = PcmReader::new(
        child.stdout.take().context("Missing FFmpeg audio pipe")?,
        4800,
    )?;
    let first = reader.read_chunk()?;
    ensure!(first.is_some(), "Replay contains no decoded audio");
    ready.send(Ok(())).ok();
    let result = (|| -> Result<()> {
        let mut chunk = first;
        while let Some(pcm) = chunk {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            send_capture(
                tx,
                CaptureEvent::Audio(cs_core::AudioChunk {
                    samples: pcm.samples,
                    sample_rate: SAMPLE_RATE,
                    timestamp_ms: pcm.timestamp_ms,
                    discontinuity: false,
                }),
                stop,
            )?;
            chunk = reader.read_chunk()?;
        }
        Ok(())
    })();
    let exit = child.finish(stop.load(Ordering::Relaxed) || result.is_err())?;
    result?;
    ensure!(
        exit.success() || stop.load(Ordering::Relaxed),
        "FFmpeg replay failed ({exit})"
    );
    Ok(())
}
fn replay_hud(
    path: &Path,
    tx: &Sender<CaptureEvent>,
    stop: &Arc<AtomicBool>,
    ocr: &mut cs_platform::HudOcr,
) -> Result<()> {
    use cs_platform::media::*;
    let interval = ocr.capture_interval_ms();
    let child = frame_command(
        &ffmpeg(),
        path,
        MediaClip::new(0.0, None)?,
        true,
        interval,
        cs_core::NormalizedRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
    )?
    .spawn()?;
    let mut child = CancellableChild::new(child, stop);
    let mut reader = PpmReader::new(
        std::io::BufReader::new(child.stdout.take().context("Missing FFmpeg HUD pipe")?),
        interval,
    )?;
    let mut frame = image::RgbaImage::new(0, 0);
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            let Some(timestamp) = reader.read_frame(&mut frame)? else {
                break;
            };
            let region = ocr.region();
            let (x, y, width, height) = region.pixels(frame.width(), frame.height())?;
            let crop = image::imageops::crop_imm(&frame, x, y, width, height).to_image();
            send_capture(
                tx,
                CaptureEvent::Hud(ocr.recognize(&crop, timestamp)?),
                stop,
            )?;
        }
        Ok(())
    })();
    let exit = child.finish(stop.load(Ordering::Relaxed) || result.is_err())?;
    result?;
    ensure!(
        exit.success() || stop.load(Ordering::Relaxed),
        "FFmpeg HUD sampler failed ({exit})"
    );
    Ok(())
}
fn send_capture(
    tx: &Sender<CaptureEvent>,
    mut event: CaptureEvent,
    stop: &AtomicBool,
) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        match tx.send_timeout(event, Duration::from_millis(25)) {
            Ok(()) => return Ok(()),
            Err(crossbeam_channel::SendTimeoutError::Timeout(value)) => event = value,
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_correction_updates_policy_without_stopping_capture_and_is_not_config() {
        let session = Session {
            stop: Arc::new(AtomicBool::new(false)),
            diagnostics: Arc::new(CaptureDiagnostics::default()),
            completion: Arc::new(Mutex::new(None)),
            policy: Arc::new(Mutex::new(None)),
        };
        let config = AppConfig::default();
        let tags = vec![cs_core::PlayerLanguage {
            player: cs_core::SpeakerLabel::new("Игрок…"),
            language: " JA_jp ".into(),
        }];
        session.update_translation(&config, &tags).unwrap();
        let policy = session.policy.lock().unwrap().take().unwrap();
        assert_eq!(policy.player_languages[0].language, "ja");
        assert_eq!(policy.player_languages[0].player, tags[0].player);
        assert!(!session.stop.load(Ordering::Relaxed));
        assert!(
            !toml::to_string(&config)
                .unwrap()
                .contains("player_languages")
        );
        session.update_translation(&config, &[]).unwrap();
        assert!(
            session
                .policy
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .player_languages
                .is_empty()
        );
        assert!(
            session
                .update_translation(&config, &[tags[0].clone(), tags[0].clone()])
                .is_err()
        );
    }
    #[test]
    fn text_only_requires_console_even_with_diagnostic_replay() {
        let config = AppConfig {
            voice_enabled: false,
            ..AppConfig::default()
        };
        let (tx, _rx) = bounded(16);
        let error = run_local(
            config,
            Some("not-a-text-source.mp4".into()),
            &tx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(CaptureDiagnostics::default()),
            Arc::new(Mutex::new(None)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("console log"));
    }

    #[test]
    #[ignore = "requires CS_TRANSLATOR_TEST_TRANSLATION_MODEL pointing to a real installed translation model"]
    fn text_only_translates_appended_unicode_without_voice_assets() -> Result<()> {
        use std::io::Write;

        let model = std::env::var_os("CS_TRANSLATOR_TEST_TRANSLATION_MODEL").context(
            "Set CS_TRANSLATOR_TEST_TRANSLATION_MODEL to the installed translation model",
        )?;
        let directory = tempfile::tempdir()?;
        let log = directory.path().join("console.log");
        std::fs::write(&log, "")?;
        let config = AppConfig {
            voice_enabled: false,
            source_language: "ru".into(),
            target_language: "en".into(),
            translation_model: model.into(),
            console_log: Some(log.clone()),
            hud_enabled: true,
            asr_manifest: directory.path().join("absent-asr.toml"),
            ocr_config: directory.path().join("absent-ocr.toml"),
            ..AppConfig::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (tx, rx) = bounded(128);
        let replay = directory.path().join("absent-replay.mp4");
        let worker = thread::spawn(move || {
            run_local(
                config,
                Some(replay),
                &tx,
                stopped,
                Arc::new(CaptureDiagnostics::default()),
                Arc::new(Mutex::new(None)),
            )
        });
        let result = (|| -> Result<Caption> {
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(
                    !remaining.is_zero(),
                    "Timed out waiting for text-only readiness"
                );
                if matches!(rx.recv_timeout(remaining)?, AppEvent::Ready) {
                    break;
                }
            }
            let mut writer = std::fs::OpenOptions::new().append(true).open(&log)?;
            writeln!(writer, "[CT] Игрок: Привет, как дела?")?;
            writer.flush()?;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(
                    !remaining.is_zero(),
                    "Timed out waiting for real chat translation"
                );
                match rx.recv_timeout(remaining)? {
                    AppEvent::Caption(caption)
                        if caption.is_final && !caption.translation_pending =>
                    {
                        return Ok(caption);
                    }
                    AppEvent::Status {
                        message,
                        is_error: true,
                        ..
                    } => bail!("{message}"),
                    _ => {}
                }
            }
        })();
        stop.store(true, Ordering::Relaxed);
        worker.join().expect("session thread panicked")?;
        let caption = result?;
        assert_eq!(caption.original, "Привет, как дела?");
        assert_eq!(caption.speakers[0].text, "Игрок");
        assert_ne!(caption.translated, caption.original);
        Ok(())
    }

    #[test]
    fn stale_translation_requires_matching_revision_source_and_policy() {
        let job = Job {
            id: 1,
            text: "Go B".into(),
            source: "en".into(),
            target: "de".into(),
            policy: 2,
            final_result: false,
        };
        assert!(matches_revision("Go B", "en", 2, &job));
        assert!(!matches_revision("Go A", "en", 2, &job));
        assert!(!matches_revision("Go B", "ru", 2, &job));
        assert!(!matches_revision("Go B", "en", 3, &job));
    }
    #[test]
    fn status_loss_is_counted() {
        let (tx, _rx) = bounded(1);
        let d = CaptureDiagnostics::default();
        status(&tx, &d, "Test", "first", false);
        status(&tx, &d, "Test", "second", false);
        assert_eq!(diagnostic_counts(&d)[3], 1);
    }
    #[test]
    fn cancellation_does_not_wait_for_full_capture_queue() {
        let (tx, _rx) = bounded(1);
        let event = || CaptureEvent::Status {
            component: "Replay".into(),
            message: "message".into(),
            is_error: false,
        };
        tx.send(event()).unwrap();
        send_capture(&tx, event(), &AtomicBool::new(true)).unwrap();
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    #[test]
    fn failed_partial_does_not_suppress_identical_final_retry() {
        let mut pending = Pending {
            caption: Caption {
                id: 1,
                start_ms: 0,
                end_ms: 100,
                channel: "voice".into(),
                speakers: vec![],
                original: "Go B".into(),
                translated: String::new(),
                translation_pending: true,
                translation_error: None,
                language: Some("en".into()),
                is_final: false,
            },
            source: "en".into(),
            submitted: Some(("Go B".into(), "en".into(), 0, false)),
        };
        pending.resolved(Err(anyhow::anyhow!("partial failed")), false);
        assert!(pending.submitted.is_none());
        assert_eq!(pending.caption.original, "Go B");
        assert!(pending.caption.translated.is_empty());
        assert_eq!(
            pending.caption.translation_error.as_deref(),
            Some("partial failed")
        );
        assert!(pending.caption.translation_pending);
        pending.caption.is_final = true;
        pending.resolved(Ok("Geh B".into()), true);
        assert_eq!(pending.caption.translated, "Geh B");
        assert!(!pending.caption.translation_pending);
        assert_eq!(pending.caption.translation_error, None);
    }
    #[test]
    fn final_translation_failure_discards_stale_partial_translation() {
        let mut pending = Pending {
            caption: Caption {
                id: 1,
                start_ms: 0,
                end_ms: 100,
                channel: "voice".into(),
                speakers: vec![],
                original: "Go B, then stop".into(),
                translated: "Geh B".into(),
                translation_pending: true,
                translation_error: None,
                language: Some("en".into()),
                is_final: true,
            },
            source: "en".into(),
            submitted: Some(("Go B, then stop".into(), "en".into(), 0, true)),
        };
        pending.resolved(Err(anyhow::anyhow!("final failed")), true);
        assert_eq!(pending.caption.original, "Go B, then stop");
        assert!(pending.caption.translated.is_empty());
        assert!(!pending.caption.translation_pending);
        assert_eq!(
            pending.caption.translation_error.as_deref(),
            Some("final failed")
        );
    }
}
