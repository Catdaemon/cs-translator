use anyhow::{Context, Result, ensure};
use crossbeam_channel::{Sender, TrySendError};
use cs_core::{CaptureDiagnostics, CaptureEvent};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub mod media;

/// A capture source's initialization and terminal result are independent of the
/// bounded, best-effort media event queue.
pub struct CaptureWorker {
    ready: crossbeam_channel::Receiver<std::result::Result<(), String>>,
    thread: JoinHandle<Result<()>>,
}

impl CaptureWorker {
    pub fn spawn(
        run: impl FnOnce(Sender<std::result::Result<(), String>>) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        capture_worker("media-replay", move |ready| run(ready.clone()))
    }

    pub fn wait_ready(&self) -> Result<()> {
        self.ready
            .recv()
            .map_err(|_| anyhow::anyhow!("Capture worker ended before initialization"))?
            .map_err(anyhow::Error::msg)
    }

    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    pub fn join(self) -> Result<()> {
        self.thread.join().map_err(|panic| {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            anyhow::anyhow!("Capture worker panicked: {message}")
        })?
    }
}

pub(crate) fn capture_worker(
    name: &str,
    run: impl FnOnce(&Sender<std::result::Result<(), String>>) -> Result<()> + Send + 'static,
) -> Result<CaptureWorker> {
    let (ready_tx, ready) = crossbeam_channel::bounded(1);
    let thread = thread::Builder::new().name(name.into()).spawn(move || {
        let result = run(&ready_tx);
        if let Err(error) = &result {
            let _ = ready_tx.try_send(Err(format!("{error:#}")));
        }
        result
    })?;
    Ok(CaptureWorker { ready, thread })
}
mod hud_settings;
mod ocr;
pub use hud_settings::{HudLayoutHint, discover_hud_layout_hint, parse_hud_layout_hint};
mod scheduling;
mod tail;
pub use ocr::HudOcr;
pub use scheduling::{SessionProcess, configure_inference_process};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{begin_overlay_move, list_windows, make_overlay, spawn_audio, spawn_hud_capture};
#[cfg(windows)]
mod discovery;
#[cfg(windows)]
pub use discovery::{discover_cs2_source, preferred_language};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameSource {
    pub process_id: u32,
    pub window: isize,
    pub console_log: Option<PathBuf>,
}

#[cfg(not(windows))]
pub fn discover_cs2_source() -> Result<Option<GameSource>> {
    anyhow::bail!("Automatic CS2 discovery requires Windows")
}

#[cfg(not(windows))]
pub fn preferred_language() -> Option<String> {
    None
}

#[cfg(not(windows))]
pub fn begin_overlay_move(_: isize) -> Result<()> {
    anyhow::bail!("Native overlay movement requires Windows")
}

#[derive(Debug, Clone)]
pub struct CaptureWindow {
    pub handle: isize,
    pub process_id: u32,
}

pub fn monotonic_ms() -> u64 {
    static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
    ORIGIN.elapsed().as_millis() as u64
}

pub(crate) fn status(
    tx: &Sender<CaptureEvent>,
    diagnostics: &CaptureDiagnostics,
    component: &str,
    message: impl Into<String>,
    is_error: bool,
) {
    deliver(
        tx,
        CaptureEvent::Status {
            component: component.into(),
            message: message.into(),
            is_error,
        },
        diagnostics,
    );
}

/// Returns false only when the consumer has disconnected. Drops are counted out of band.
pub(crate) fn deliver(
    tx: &Sender<CaptureEvent>,
    event: CaptureEvent,
    diagnostics: &CaptureDiagnostics,
) -> bool {
    match tx.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Disconnected(_)) => false,
        Err(TrySendError::Full(event)) => {
            let counter = match event {
                CaptureEvent::Audio(_) => &diagnostics.audio_packets_dropped,
                CaptureEvent::Hud(_) => &diagnostics.hud_observations_dropped,
                CaptureEvent::Chat(_) => &diagnostics.chat_messages_dropped,
                CaptureEvent::Status { .. } => &diagnostics.status_messages_dropped,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            true
        }
    }
}

#[cfg(any(windows, test))]
pub(crate) fn deliver_audio(
    tx: &Sender<CaptureEvent>,
    mut chunk: cs_core::AudioChunk,
    pending_discontinuity: &mut bool,
    diagnostics: &CaptureDiagnostics,
) -> bool {
    chunk.discontinuity |= *pending_discontinuity;
    match tx.try_send(CaptureEvent::Audio(chunk)) {
        Ok(()) => {
            *pending_discontinuity = false;
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
        Err(TrySendError::Full(_)) => {
            diagnostics
                .audio_packets_dropped
                .fetch_add(1, Ordering::Relaxed);
            *pending_discontinuity = true;
            true
        }
    }
}

pub fn spawn_log_tail(
    path: PathBuf,
    tx: Sender<CaptureEvent>,
    stop: Arc<AtomicBool>,
    diagnostics: Arc<CaptureDiagnostics>,
) -> Result<CaptureWorker> {
    ensure!(
        tx.capacity().is_some(),
        "Capture requires a bounded event channel"
    );
    capture_worker("console-tail", move |ready| {
        tail::follow(&path, &tx, &stop, &diagnostics, ready)
    })
}

#[cfg(not(windows))]
pub fn list_windows() -> Result<Vec<CaptureWindow>> {
    anyhow::bail!("Window enumeration requires the Windows platform adapter")
}
#[cfg(not(windows))]
pub fn make_overlay(_: isize, _: bool, _: (i32, i32)) -> Result<()> {
    anyhow::bail!("Native overlay requires the Windows platform adapter")
}
#[cfg(not(windows))]
pub fn spawn_audio(
    _: u32,
    _: Sender<CaptureEvent>,
    _: Arc<AtomicBool>,
    _: Arc<CaptureDiagnostics>,
) -> Result<CaptureWorker> {
    anyhow::bail!("Process loopback requires Windows 10 build 20348 or newer")
}
/// Bounded standard HUD search area, including horizontal safe-area insets.
/// Nonstandard layouts can supply an explicit area override.
pub fn standard_speaker_region() -> cs_core::NormalizedRect {
    cs_core::NormalizedRect {
        x: 0.0,
        y: 0.55,
        width: 0.55,
        height: 0.40,
    }
}

#[cfg(not(windows))]
pub fn spawn_hud_capture(
    _: isize,
    _: PathBuf,
    _: Sender<CaptureEvent>,
    _: Arc<AtomicBool>,
    _: Arc<CaptureDiagnostics>,
) -> Result<CaptureWorker> {
    anyhow::bail!("Live HUD capture requires Windows Graphics Capture; replay OCR is available")
}

pub(crate) fn pause(stop: &AtomicBool, duration: Duration) {
    let end = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
}

pub(crate) fn read_config(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| {
        format!(
            "Read OCR configuration {} (see crates/platform/ocr.example.toml)",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_readiness_and_terminal_errors_are_not_media_events() {
        let worker = capture_worker("readiness-test", |ready| {
            ready.send(Ok(()))?;
            anyhow::bail!("terminal failure")
        })
        .unwrap();
        worker.wait_ready().unwrap();
        assert!(
            worker
                .join()
                .unwrap_err()
                .to_string()
                .contains("terminal failure")
        );
        let worker = capture_worker("initialization-test", |_| {
            anyhow::bail!("initialization failure")
        })
        .unwrap();
        assert!(
            worker
                .wait_ready()
                .unwrap_err()
                .to_string()
                .contains("initialization failure")
        );
        assert!(worker.join().is_err());
        let worker = capture_worker("panic-test", |_| panic!("capture panic")).unwrap();
        assert!(worker.wait_ready().is_err());
        assert!(
            worker
                .join()
                .unwrap_err()
                .to_string()
                .contains("capture panic")
        );
    }
    #[test]
    fn full_queue_counts_media_and_status_outside_queue() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let diagnostics = CaptureDiagnostics::default();
        status(&tx, &diagnostics, "test", "occupied", false);
        assert!(deliver(
            &tx,
            CaptureEvent::Hud(cs_core::HudObservation {
                timestamp_ms: 1,
                players: Vec::new(),
            }),
            &diagnostics
        ));
        status(&tx, &diagnostics, "test", "error", true);
        assert_eq!(
            diagnostics.hud_observations_dropped.load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            diagnostics.status_messages_dropped.load(Ordering::Relaxed),
            1
        );
        rx.recv().unwrap();
        assert!(deliver(
            &tx,
            CaptureEvent::Hud(cs_core::HudObservation {
                timestamp_ms: 2,
                players: Vec::new(),
            }),
            &diagnostics
        ));
        assert!(matches!(rx.recv().unwrap(), CaptureEvent::Hud(_)));
        drop(rx);
        assert!(!deliver(
            &tx,
            CaptureEvent::Hud(cs_core::HudObservation {
                timestamp_ms: 3,
                players: Vec::new(),
            }),
            &diagnostics
        ));
    }
    #[test]
    fn dropped_gap_survives_until_next_successful_audio_packet() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let diagnostics = CaptureDiagnostics::default();
        let mut pending = false;
        let chunk = |timestamp_ms, discontinuity| cs_core::AudioChunk {
            samples: vec![0.0; 480],
            sample_rate: 48000,
            timestamp_ms,
            discontinuity,
        };
        assert!(deliver_audio(
            &tx,
            chunk(100, false),
            &mut pending,
            &diagnostics
        ));
        assert!(deliver_audio(
            &tx,
            chunk(110, true),
            &mut pending,
            &diagnostics
        ));
        assert!(pending);
        rx.recv().unwrap();
        assert!(deliver_audio(
            &tx,
            chunk(150, false),
            &mut pending,
            &diagnostics
        ));
        let CaptureEvent::Audio(next) = rx.recv().unwrap() else {
            panic!("expected audio");
        };
        assert!(next.discontinuity);
        assert_eq!(next.timestamp_ms, 150);
        assert!(!pending);
        assert_eq!(diagnostics.audio_packets_dropped.load(Ordering::Relaxed), 1);
    }
}
