use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::{SendTimeoutError, Sender, TrySendError, bounded};
use cs_core::{AppConfig, AppEvent, CaptureDiagnostics};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(25);
const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Start {
    config: AppConfig,
    replay: Option<PathBuf>,
}
#[derive(Serialize, Deserialize)]
enum Control {
    Cancel,
    Policy(crate::session::TranslationPolicy),
}
#[derive(Serialize, Deserialize)]
enum Message {
    Event(AppEvent),
    Drops([u64; 4]),
    Finished(Option<String>),
}

fn write_frame(writer: &mut impl Write, value: &impl Serialize, bytes: &mut Vec<u8>) -> Result<()> {
    bytes.clear();
    serde_json::to_writer(&mut *bytes, value)?;
    ensure!(
        bytes.len() <= MAX_FRAME,
        "Session IPC frame exceeds size limit"
    );
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}
fn read_frame<T: DeserializeOwned>(reader: &mut impl Read, bytes: &mut Vec<u8>) -> Result<T> {
    let mut size = [0; 4];
    reader
        .read_exact(&mut size)
        .context("reading session IPC frame header")?;
    let size = u32::from_le_bytes(size) as usize;
    ensure!(size <= MAX_FRAME, "Session IPC frame exceeds size limit");
    bytes.resize(size, 0);
    reader
        .read_exact(bytes)
        .context("reading session IPC frame body")?;
    serde_json::from_slice(bytes).context("decoding session IPC frame")
}
fn counters(d: &CaptureDiagnostics) -> [&std::sync::atomic::AtomicU64; 4] {
    [
        &d.audio_packets_dropped,
        &d.hud_observations_dropped,
        &d.chat_messages_dropped,
        &d.status_messages_dropped,
    ]
}

// Cancellation stops production, not delivery of captions already read from IPC.
// The desktop drops its receiver on close, which also releases a full queue.
fn forward_caption(tx: &Sender<AppEvent>, event: AppEvent, stop: &AtomicBool) {
    if tx.send(event).is_err() {
        stop.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn run_parent(
    config: AppConfig,
    replay: Option<PathBuf>,
    tx: &Sender<AppEvent>,
    stop: Arc<AtomicBool>,
    diagnostics: Arc<CaptureDiagnostics>,
    policy: Arc<Mutex<Option<crate::session::TranslationPolicy>>>,
) -> Result<()> {
    ensure!(
        tx.capacity().is_some(),
        "Session requires bounded event delivery"
    );
    let mut command = Command::new(std::env::current_exe().context("locating session executable")?);
    command
        .arg("--session-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut process = cs_platform::SessionProcess::spawn(&mut command)?;
    let mut control = process
        .child
        .stdin
        .take()
        .context("missing session control pipe")?;
    let mut output = process
        .child
        .stdout
        .take()
        .context("missing session output pipe")?;
    // Job ownership is established before the child receives permission to start capture/models.
    write_frame(&mut control, &Start { config, replay }, &mut Vec::new())?;
    let (messages, received) = bounded(64);
    let reader_stop = Arc::new(AtomicBool::new(false));
    let stopped = reader_stop.clone();
    let reader = thread::spawn(move || {
        let mut frame = Vec::new();
        loop {
            let message = read_frame::<Message>(&mut output, &mut frame);
            let terminal = message.is_err() || matches!(&message, Ok(Message::Finished(_)));
            let mut pending = message;
            loop {
                match messages.send_timeout(pending, POLL) {
                    Ok(()) => break,
                    Err(SendTimeoutError::Timeout(value)) if !stopped.load(Ordering::Relaxed) => {
                        pending = value
                    }
                    Err(_) => return,
                }
            }
            if terminal {
                return;
            }
        }
    });
    let result = (|| -> Result<()> {
        let mut stopping = None;
        let mut finished = None;
        loop {
            if let Some(update) = policy.lock().unwrap().take() {
                write_frame(&mut control, &Control::Policy(update), &mut Vec::new())?;
            }
            if stop.load(Ordering::Relaxed) && stopping.is_none() {
                let _ = write_frame(&mut control, &Control::Cancel, &mut Vec::new());
                stopping = Some(Instant::now());
            }
            if stopping.is_some_and(|time: Instant| time.elapsed() >= Duration::from_secs(3))
                && process.child.try_wait()?.is_none()
            {
                if let Err(error) = process.child.kill() {
                    // Exit can race even the preceding nonblocking reap.
                    if process.child.try_wait()?.is_none() {
                        return Err(error).context("terminating unresponsive session worker");
                    }
                }
                process
                    .child
                    .wait()
                    .context("reaping cancelled session worker")?;
            }
            match received.recv_timeout(POLL) {
                Ok(Ok(Message::Event(event))) => {
                    if matches!(&event, AppEvent::Status { .. }) {
                        match tx.try_send(event) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                diagnostics
                                    .status_messages_dropped
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                stop.store(true, Ordering::Relaxed);
                            }
                        }
                    } else {
                        forward_caption(tx, event, &stop);
                    }
                }
                Ok(Ok(Message::Drops(delta))) => {
                    for (counter, delta) in counters(&diagnostics).into_iter().zip(delta) {
                        counter.fetch_add(delta, Ordering::Relaxed);
                    }
                }
                Ok(Ok(Message::Finished(error))) => {
                    finished = Some(error);
                }
                Ok(Err(error)) if finished.is_none() => {
                    if stopping.is_some() {
                        return Ok(());
                    }
                    return Err(error).context("session worker IPC failed");
                }
                Ok(Err(_)) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) if stopping.is_some() => {
                    return Ok(());
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) if finished.is_none() => {
                    bail!("Session worker output disconnected before completion")
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    thread::sleep(POLL);
                }
            }
            // Drain the reader's final messages before interpreting process exit.
            if let Some(status) = process.child.try_wait()?
                && let Some(error) = finished
            {
                ensure!(status.success(), "Session worker exited with {status}");
                if let Some(error) = error {
                    bail!("{error}");
                }
                return Ok(());
            }
        }
    })();
    reader_stop.store(true, Ordering::Relaxed);
    drop(control);
    // Closing the job also reaps descendants after success or worker failure.
    drop(process);
    let _ = reader.join();
    result
}

pub(crate) fn run_child() -> Result<()> {
    let mut input = std::io::stdin();
    let Start { config, replay } = read_frame(&mut input, &mut Vec::new())?;
    cs_platform::configure_inference_process(config.background_inference)?;
    let stop = Arc::new(AtomicBool::new(false));
    let cancelled = stop.clone();
    let policy = Arc::new(Mutex::new(None));
    let updates = policy.clone();
    // Detached until process exit: control reads must not obstruct normal EOF.
    thread::spawn(move || {
        let mut frame = Vec::new();
        loop {
            match read_frame::<Control>(&mut input, &mut frame) {
                Ok(Control::Policy(update)) => *updates.lock().unwrap() = Some(update),
                Ok(Control::Cancel) | Err(_) => {
                    cancelled.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });
    let diagnostics = Arc::new(CaptureDiagnostics::default());
    let (events, received) = bounded(128);
    let worker_stop = stop.clone();
    let worker_diagnostics = diagnostics.clone();
    let worker = thread::spawn(move || {
        crate::session::run_local(
            config,
            replay,
            &events,
            worker_stop,
            worker_diagnostics,
            policy,
        )
    });
    let mut output = std::io::stdout().lock();
    let mut previous = [0_u64; 4];
    let mut frame = Vec::new();
    let transport = (|| -> Result<()> {
        loop {
            match received.recv_timeout(POLL) {
                Ok(event) => write_frame(&mut output, &Message::Event(event), &mut frame)?,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            report_drops(&diagnostics, &mut previous, &mut output, &mut frame)?;
        }
        report_drops(&diagnostics, &mut previous, &mut output, &mut frame)
    })();
    if transport.is_err() {
        stop.store(true, Ordering::Relaxed);
    }
    let result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("Session worker thread panicked"))
        .and_then(|result| result);
    transport?;
    write_frame(
        &mut output,
        &Message::Finished(result.err().map(|error| format!("{error:#}"))),
        &mut frame,
    )
}

fn report_drops(
    diagnostics: &CaptureDiagnostics,
    previous: &mut [u64; 4],
    output: &mut impl Write,
    frame: &mut Vec<u8>,
) -> Result<()> {
    let mut delta = [0; 4];
    for (i, counter) in counters(diagnostics).into_iter().enumerate() {
        let current = counter.load(Ordering::Relaxed);
        delta[i] = current.saturating_sub(previous[i]);
        previous[i] = current;
    }
    if delta != [0; 4] {
        write_frame(output, &Message::Drops(delta), frame)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use cs_core::Caption;
    use std::collections::VecDeque;

    fn caption(is_final: bool) -> AppEvent {
        AppEvent::Caption(Caption {
            id: 1,
            start_ms: 0,
            end_ms: 1,
            channel: "voice".into(),
            speakers: vec![],
            original: if is_final { "final" } else { "partial" }.into(),
            translated: String::new(),
            translation_pending: false,
            translation_error: None,
            language: None,
            is_final,
        })
    }

    #[test]
    fn stopped_consumer_keeps_buffered_final_in_history() {
        let (tx, rx) = bounded(1);
        tx.send(caption(false)).unwrap();
        let stop = Arc::new(AtomicBool::new(true));
        let worker = thread::spawn(move || forward_caption(&tx, caption(true), &stop));
        let mut history = VecDeque::new();
        for _ in 0..2 {
            let AppEvent::Caption(caption) = rx.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected caption");
            };
            crate::upsert(&mut history, caption);
        }
        worker.join().unwrap();
        // Completion removes unfinished partials only, as the desktop consumer does.
        history.retain(|caption| caption.is_final);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].original, "final");
    }

    #[test]
    fn closing_full_consumer_queue_releases_forwarder() {
        let (tx, rx) = bounded(1);
        tx.send(caption(false)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (done, finished) = bounded(1);
        let worker = thread::spawn(move || {
            forward_caption(&tx, caption(true), &worker_stop);
            done.send(()).unwrap();
        });
        drop(rx);
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(stop.load(Ordering::Relaxed));
    }
}
