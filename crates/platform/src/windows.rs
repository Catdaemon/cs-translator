use crate::{
    CaptureWindow, CaptureWorker, HudOcr, capture_worker, deliver, monotonic_ms, pause, status,
};
use anyhow::{Context as _, Result, ensure};
use crossbeam_channel::{Receiver, Sender};
use cs_core::{AudioChunk, CaptureDiagnostics, CaptureEvent, HudObservation, NormalizedRect};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, POINT, RECT, SetLastError, WIN32_ERROR, WPARAM},
    UI::Input::KeyboardAndMouse::ReleaseCapture,
    UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetCursorPos, GetWindowLongPtrW,
        GetWindowThreadProcessId, HTCAPTION, HWND_TOPMOST, PostMessageW, SC_MOVE, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WM_SYSCOMMAND,
        WS_BORDER, WS_CAPTION, WS_DLGFRAME, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TRANSPARENT, WS_THICKFRAME,
    },
};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

pub fn list_windows() -> Result<Vec<CaptureWindow>> {
    Ok(Window::enumerate()?
        .into_iter()
        .filter_map(|window| {
            Some(CaptureWindow {
                handle: window.as_raw_hwnd() as isize,
                process_id: window.process_id().ok()?,
            })
        })
        .collect())
}

pub fn make_overlay(window_handle: isize, click_through: bool, origin: (i32, i32)) -> Result<()> {
    ensure!(window_handle != 0, "Overlay native HWND is null");
    let hwnd = HWND(window_handle as *mut std::ffi::c_void);
    // Only this app's independent native window is modified. No hooks or game access.
    unsafe {
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client)?;
        let mut style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        style |= WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0 | WS_EX_LAYERED.0;
        if click_through {
            style |= WS_EX_TRANSPARENT.0;
        } else {
            style &= !WS_EX_TRANSPARENT.0;
        }
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
        if previous == 0 && GetLastError().0 != 0 {
            return Err(windows::core::Error::from_win32().into());
        }
        // GPUI's hidden titlebar can still retain a native non-client frame.
        // The movement outline is drawn by our view, never by Windows.
        // Keep the requested client rectangle when removing the non-client frame.
        // Otherwise each close/reopen saves that frame as extra overlay content.
        let frame_style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32
            & !(WS_CAPTION.0 | WS_THICKFRAME.0 | WS_BORDER.0 | WS_DLGFRAME.0);
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(hwnd, GWL_STYLE, frame_style as isize);
        if previous == 0 && GetLastError().0 != 0 {
            return Err(windows::core::Error::from_win32().into());
        }
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            origin.0,
            origin.1,
            client.right - client.left,
            client.bottom - client.top,
            SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;
    }
    Ok(())
}

/// Resize our borderless overlay without reintroducing GPUI's cached frame insets.
pub fn resize_overlay(window_handle: isize, width: i32) -> Result<()> {
    ensure!(width > 0, "Overlay width must be positive");
    let hwnd = HWND(window_handle as *mut std::ffi::c_void);
    let mut process_id = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    ensure!(
        process_id == std::process::id(),
        "Only this application's overlay can be resized"
    );
    unsafe {
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client)?;
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width,
            client.bottom - client.top,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER,
        )?;
    }
    Ok(())
}

pub fn begin_overlay_move(window_handle: isize) -> Result<()> {
    let hwnd = HWND(window_handle as *mut std::ffi::c_void);
    let mut process_id = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    ensure!(
        process_id == std::process::id(),
        "Only this application's overlay can be moved"
    );
    let mut position = POINT::default();
    unsafe {
        GetCursorPos(&mut position)?;
        // No previous capture is fine. Only our own window receives this message.
        let _ = ReleaseCapture();
        let packed = ((position.y as u32) << 16) | u32::from(position.x as u16);
        // The GPUI mouse callback still holds the window/app borrow here.
        // Use the system command: GPUI routes non-client mouse messages back
        // through the same mouse listener. This also avoids that extra dispatch.
        PostMessageW(
            Some(hwnd),
            WM_SYSCOMMAND,
            WPARAM((SC_MOVE | HTCAPTION) as usize),
            LPARAM(packed as isize),
        )?;
    }
    Ok(())
}

pub fn spawn_audio(
    process_id: u32,
    tx: Sender<CaptureEvent>,
    stop: Arc<AtomicBool>,
    diagnostics: Arc<CaptureDiagnostics>,
) -> Result<CaptureWorker> {
    ensure!(
        process_id != 0,
        "Choose the CS2 process id, not desktop loopback"
    );
    ensure!(tx.capacity().is_some(), "Capture requires bounded channels");
    let _ = monotonic_ms();
    capture_worker("process-audio", move |ready| {
        audio_loop(process_id, &tx, &stop, &diagnostics, ready)
    })
}

fn audio_loop(
    process_id: u32,
    tx: &Sender<CaptureEvent>,
    stop: &AtomicBool,
    diagnostics: &CaptureDiagnostics,
    ready: &Sender<std::result::Result<(), String>>,
) -> Result<()> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};
    crate::scheduling::restore_capture_priority()?;
    wasapi::initialize_mta()
        .ok()
        .context("Initialize capture COM apartment")?;
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }
    let _com = Com;
    let mut client = AudioClient::new_application_loopback_client(process_id, true)
        .context("Activate process-specific WASAPI loopback (requires Windows build 20348+)")?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48000, 2, None);
    client
        .initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::PollingShared {
                autoconvert: true,
                buffer_duration_hns: 200_000,
            },
        )
        .context("Initialize process loopback PCM")?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    let origin_ms = monotonic_ms();
    let _ = ready.try_send(Ok(()));
    let mut frame_cursor = 0_u64;
    let mut pending_discontinuity = false;
    status(
        tx,
        diagnostics,
        "audio",
        format!(
            "Capturing PID {process_id} process tree only, 48 kHz float stereo downmixed to mono"
        ),
        false,
    );
    let mut bytes = Vec::with_capacity(48000 * 8 / 10);
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            while let Some(frames) = capture.get_next_packet_size()? {
                if frames == 0 {
                    break;
                }
                ensure!(
                    frames <= 48000 * 2,
                    "Process audio packet unexpectedly exceeds two seconds"
                );
                bytes.resize(frames as usize * 8, 0);
                let (read, flags) = capture.read_from_device(&mut bytes)?;
                if flags.data_discontinuity {
                    // The wrapper does not expose packet device/QPC positions. Reanchor only
                    // on a native gap; ordinary queued packets retain consecutive frame time.
                    let observed_start =
                        monotonic_ms().saturating_sub(u64::from(read) * 1000 / 48000);
                    frame_cursor = frame_cursor.max(observed_start.saturating_sub(origin_ms) * 48);
                }
                let timestamp_ms = origin_ms + frame_cursor * 1000 / 48000;
                frame_cursor += u64::from(read);
                let mut samples = Vec::with_capacity(read as usize);
                if flags.silent {
                    samples.resize(read as usize, 0.0);
                } else {
                    for frame in bytes[..read as usize * 8].as_chunks::<8>().0 {
                        let left = f32::from_le_bytes(frame[..4].try_into()?);
                        let right = f32::from_le_bytes(frame[4..].try_into()?);
                        samples.push((left + right) * 0.5);
                    }
                }
                pending_discontinuity |= flags.data_discontinuity;
                let chunk = AudioChunk {
                    samples,
                    sample_rate: 48000,
                    timestamp_ms,
                    discontinuity: pending_discontinuity,
                };
                if !crate::deliver_audio(tx, chunk, &mut pending_discontinuity, diagnostics) {
                    return Ok(());
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            pause(stop, Duration::from_millis(10));
        }
        Ok(())
    })();
    let stopped = client.stop_stream().context("Stop WASAPI process capture");
    result.and(stopped)
}

struct FrameFlags {
    roi: NormalizedRect,
    frames: Sender<(image::RgbaImage, u64)>,
    pending_frames: Receiver<(image::RgbaImage, u64)>,
    stop: Arc<AtomicBool>,
    interval_ms: u64,
    events: Sender<CaptureEvent>,
    diagnostics: Arc<CaptureDiagnostics>,
    failure: Sender<String>,
}

struct HudCapture {
    flags: FrameFlags,
    last_frame: Instant,
    scratch: Vec<u8>,
}
impl GraphicsCaptureApiHandler for HudCapture {
    type Flags = FrameFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            flags: ctx.flags,
            last_frame: Instant::now() - Duration::from_secs(1),
            scratch: Vec::new(),
        })
    }
    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        if self.flags.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        if self.last_frame.elapsed().as_millis() < self.flags.interval_ms as u128 {
            return Ok(());
        }
        self.last_frame = Instant::now();
        let timestamp = frame_timestamp_ms(frame.timestamp()?.Duration)?;
        let (x, y, width, height) = self
            .flags
            .roi
            .pixels(frame.width(), frame.height())
            .map_err(|e| -> Self::Error { e.into() })?;
        // buffer_crop issues a GPU region copy before staging/readback, not full-frame CPU cropping.
        let buffer = frame.buffer_crop(x, y, x + width, y + height)?;
        let bytes = buffer.as_nopadding_buffer(&mut self.scratch);
        // OCR owns at most one in-flight image and one pending image. Reuse
        // the replaced image's allocation when OCR cannot keep up.
        if replace_pending_frame(&self.flags.frames, &self.flags.pending_frames, |pending| {
            let mut pixels = pending.map_or_else(Vec::new, |(image, _)| image.into_raw());
            pixels.clear();
            pixels.extend_from_slice(bytes);
            let image = image::RgbaImage::from_raw(width, height, pixels)
                .ok_or_else(|| anyhow::anyhow!("Captured frame buffer has invalid dimensions"))?;
            Ok((image, timestamp))
        })? {
            self.flags
                .diagnostics
                .hud_frames_coalesced
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
    fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
        let _ = self.flags.failure.try_send(
            "Captured window closed; choose a live game window and restart capture".into(),
        );
        status(
            &self.flags.events,
            &self.flags.diagnostics,
            "hud",
            "Captured window closed",
            true,
        );
        Ok(())
    }
}

fn replace_pending_frame<T>(
    sender: &Sender<T>,
    receiver: &Receiver<T>,
    next: impl FnOnce(Option<T>) -> Result<T>,
) -> Result<bool> {
    // There is one capture producer. Taking the pending value first permits
    // buffer reuse; the OCR consumer may already have taken it.
    let pending = receiver.try_recv().ok();
    let replaced = pending.is_some();
    sender
        .try_send(next(pending)?)
        .map_err(|_| anyhow::anyhow!("Publish latest HUD frame failed"))?;
    Ok(replaced)
}

fn hud_border_settings(
    tx: &Sender<CaptureEvent>,
    diagnostics: &CaptureDiagnostics,
    stop: &AtomicBool,
) -> DrawBorderSettings {
    use windows::{
        Graphics::Capture::{GraphicsCaptureAccess, GraphicsCaptureAccessKind},
        Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus,
    };
    let request = (|| -> Result<()> {
        if !windows_capture::graphics_capture_api::GraphicsCaptureApi::is_border_settings_supported(
        )? {
            anyhow::bail!(
                "This Windows version does not support borderless Graphics Capture (build 20348+ required)"
            );
        }
        let operation =
            GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)?;
        let started = Instant::now();
        // WinRT AsyncStatus::Started is zero. Poll only on this HUD worker,
        // never blocking UI/audio, and do not wait indefinitely on consent.
        while operation.Status()?.0 == 0 {
            if stop.load(Ordering::Relaxed) || started.elapsed() >= Duration::from_secs(5) {
                operation
                    .Cancel()
                    .context("Cancel pending borderless capture consent")?;
                anyhow::bail!(
                    "Borderless capture consent was cancelled or timed out; restart Speaker detection to request access again"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let access = operation.GetResults()?;
        if access != AppCapabilityAccessStatus::Allowed {
            anyhow::bail!(
                "Windows denied borderless capture ({access:?}); check Windows capture privacy/policy permissions. Packaged apps also require graphicsCaptureWithoutBorder"
            );
        }
        Ok(())
    })();
    match request {
        Ok(()) => {
            status(
                tx,
                diagnostics,
                "hud-border",
                "Windows permitted borderless capture; requesting no border for every session. Other capture apps or OS policy can still require a border",
                false,
            );
            DrawBorderSettings::WithoutBorder
        }
        Err(error) => {
            // Permission failure is not capture failure. Keep the permitted
            // bordered session, and expose the actual denial/error to the UI.
            status(
                tx,
                diagnostics,
                "hud-border",
                format!(
                    "Borderless capture unavailable: {error:#}. Speaker detection continues with the Windows capture border"
                ),
                true,
            );
            DrawBorderSettings::Default
        }
    }
}

pub fn spawn_hud_capture(
    window: isize,
    ocr_config: PathBuf,
    tx: Sender<CaptureEvent>,
    stop: Arc<AtomicBool>,
    diagnostics: Arc<CaptureDiagnostics>,
) -> Result<CaptureWorker> {
    ensure!(window != 0, "Choose a valid HUD capture window");
    ensure!(tx.capacity().is_some(), "Capture requires bounded channels");
    let _ = monotonic_ms();
    capture_worker("hud-ocr", move |ready| {
        hud_loop(window, &ocr_config, &tx, &stop, &diagnostics, ready)
    })
}

fn hud_loop(
    window: isize,
    config: &std::path::Path,
    tx: &Sender<CaptureEvent>,
    stop: &Arc<AtomicBool>,
    diagnostics: &Arc<CaptureDiagnostics>,
    ready: &Sender<std::result::Result<(), String>>,
) -> Result<()> {
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
    unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
    struct WinRtApartment;
    impl Drop for WinRtApartment {
        fn drop(&mut self) {
            unsafe { RoUninitialize() };
        }
    }
    let _apartment = WinRtApartment;
    let mut ocr = HudOcr::load(config)?;
    let hint = crate::discover_hud_layout_hint();
    ocr.set_layout_hint(hint);
    status(
        tx,
        diagnostics,
        "hud",
        if hint.is_some() {
            "Saved CS2 HUD settings found; actual speaker glyph confirmation required"
        } else {
            "Saved CS2 HUD hints unavailable; using automatic glyph search"
        },
        false,
    );
    let (frames, receiver) = crossbeam_channel::bounded(1);
    let (failure, failures) = crossbeam_channel::bounded(1);
    let target = Window::from_raw_hwnd(window as *mut std::ffi::c_void);
    ensure!(target.is_valid(), "Selected HUD window no longer exists");
    let bounds = target.rect()?;
    let mut original_size = (bounds.right - bounds.left, bounds.bottom - bounds.top);
    let flags = FrameFlags {
        roi: ocr.region(),
        frames: frames.clone(),
        pending_frames: receiver.clone(),
        stop: stop.clone(),
        interval_ms: ocr.capture_interval_ms(),
        events: tx.clone(),
        diagnostics: diagnostics.clone(),
        failure: failure.clone(),
    };
    let minimum_interval =
        if windows_capture::graphics_capture_api::GraphicsCaptureApi::is_minimum_update_interval_supported()? {
            MinimumUpdateIntervalSettings::Custom(Duration::from_millis(ocr.capture_interval_ms()))
        } else {
            MinimumUpdateIntervalSettings::Default
        };
    let border = hud_border_settings(tx, diagnostics, stop);
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let settings = Settings::new(
        target,
        CursorCaptureSettings::WithoutCursor,
        border,
        SecondaryWindowSettings::Default,
        minimum_interval,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        flags,
    );
    let mut control = Some(
        HudCapture::start_free_threaded(settings)
            .map_err(|error| anyhow::anyhow!("Start Windows Graphics Capture: {error}"))?,
    );
    let mut initialized = false;
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            if control
                .as_ref()
                .is_some_and(|session| session.is_finished())
            {
                control
                    .take()
                    .expect("owned capture session")
                    .wait()
                    .map_err(|error| anyhow::anyhow!("Windows Graphics Capture failed: {error}"))?;
                anyhow::bail!(
                    "{}",
                    failures.try_recv().unwrap_or_else(|_| {
                        "Windows capture ended unexpectedly; restart Speaker detection".into()
                    })
                );
            }
            // A resized or idle WGC session can retain its old texture allocation
            // and stop delivering frames, so supervise source geometry here too.
            ensure!(target.is_valid(), "Captured window closed");
            let bounds = target.rect()?;
            let size = (bounds.right - bounds.left, bounds.bottom - bounds.top);
            if size != original_size && size.0 > 0 && size.1 > 0 {
                let old = control.take().expect("owned capture session");
                old.stop()
                    .map_err(|error| anyhow::anyhow!("Stop resized Windows capture: {error}"))?;
                if receiver.try_recv().is_ok() {
                    diagnostics
                        .hud_frames_coalesced
                        .fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(error) = failures.try_recv() {
                    anyhow::bail!("{error}");
                }
                ocr.invalidate_layout();
                deliver(
                    tx,
                    CaptureEvent::Hud(HudObservation {
                        timestamp_ms: monotonic_ms(),
                        players: Vec::new(),
                    }),
                    diagnostics,
                );
                original_size = size;
                let flags = FrameFlags {
                    roi: ocr.region(),
                    frames: frames.clone(),
                    pending_frames: receiver.clone(),
                    stop: stop.clone(),
                    interval_ms: ocr.capture_interval_ms(),
                    events: tx.clone(),
                    diagnostics: diagnostics.clone(),
                    failure: failure.clone(),
                };
                let settings = Settings::new(
                    target,
                    CursorCaptureSettings::WithoutCursor,
                    border,
                    SecondaryWindowSettings::Default,
                    minimum_interval,
                    DirtyRegionSettings::Default,
                    ColorFormat::Rgba8,
                    flags,
                );
                control = Some(HudCapture::start_free_threaded(settings).map_err(|error| {
                    anyhow::anyhow!("Recreate resized Windows capture: {error}")
                })?);
                continue;
            }
            if let Ok(error) = failures.try_recv() {
                anyhow::bail!("{error}");
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok((image, timestamp)) => {
                    let observation = ocr.recognize(&image, timestamp)?;
                    if !initialized {
                        let _ = ready.try_send(Ok(()));
                        status(
                            tx,
                            diagnostics,
                            "hud",
                            "Passive Windows Graphics Capture and local multilingual OCR active",
                            false,
                        );
                        initialized = true;
                    }
                    if !deliver(tx, CaptureEvent::Hud(observation), diagnostics) {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    })();
    if let Err(error) = &result {
        status(tx, diagnostics, "hud", format!("{error:#}"), true);
    }
    let _ = deliver(
        tx,
        CaptureEvent::Hud(HudObservation {
            timestamp_ms: monotonic_ms(),
            players: Vec::new(),
        }),
        diagnostics,
    );
    let stopped = if let Some(control) = control {
        control
            .stop()
            .map_err(|error| anyhow::anyhow!("Stop Windows Graphics Capture: {error}"))
    } else {
        Ok(())
    };
    result.and(stopped)
}

/// WGC SystemRelativeTime is QPC expressed in 100 ns units. Map it to the
/// process-wide monotonic epoch shared by audio, retaining capture (not callback) time.
fn frame_timestamp_ms(system_relative_100ns: i64) -> Result<u64> {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    static EPOCH: std::sync::OnceLock<(i64, u64)> = std::sync::OnceLock::new();
    let (qpc_100ns, epoch_ms) = if let Some(epoch) = EPOCH.get() {
        *epoch
    } else {
        let mut counter = 0;
        let mut frequency = 0;
        unsafe {
            QueryPerformanceFrequency(&mut frequency)?;
            QueryPerformanceCounter(&mut counter)?;
        }
        ensure!(frequency > 0, "Invalid performance counter frequency");
        let epoch = (
            (i128::from(counter) * 10_000_000 / i128::from(frequency)) as i64,
            monotonic_ms(),
        );
        *EPOCH.get_or_init(|| epoch)
    };
    Ok(
        (i128::from(epoch_ms)
            + (i128::from(system_relative_100ns) - i128::from(qpc_100ns)) / 10_000)
            .clamp(0, i128::from(u64::MAX)) as u64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_resizing_preserves_borderless_height_and_position() -> Result<()> {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetWindowRect, WINDOW_EX_STYLE, WS_POPUP,
        };

        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }

        let window = TestWindow(unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::w!("STATIC"),
                windows::core::w!("Overlay geometry regression"),
                WS_POPUP | WS_BORDER,
                17,
                23,
                560,
                150,
                None,
                None,
                None,
                None,
            )?
        });
        make_overlay(window.0.0 as isize, true, (17, 23))?;
        let mut initial = RECT::default();
        unsafe {
            GetWindowRect(window.0, &mut initial)?;
        }

        for width in [320, 240, 1600, 320] {
            resize_overlay(window.0.0 as isize, width)?;
            let (mut bounds, mut client) = (RECT::default(), RECT::default());
            unsafe {
                GetWindowRect(window.0, &mut bounds)?;
                GetClientRect(window.0, &mut client)?;
            }
            assert_eq!(bounds.right - bounds.left, width);
            assert_eq!(client.right - client.left, width);
            assert_eq!(bounds.bottom - bounds.top, initial.bottom - initial.top);
            assert_eq!(client.bottom - client.top, initial.bottom - initial.top);
            assert_eq!((bounds.left, bounds.top), (initial.left, initial.top));
        }
        Ok(())
    }

    #[test]
    fn slow_ocr_keeps_only_latest_original_timestamp() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        assert!(!replace_pending_frame(&sender, &receiver, |_| Ok(("first", 100_u64))).unwrap());
        let in_flight = receiver.recv().unwrap();
        assert!(!replace_pending_frame(&sender, &receiver, |_| Ok(("pending", 200))).unwrap());
        for timestamp in [300, 400, 500] {
            assert!(
                replace_pending_frame(&sender, &receiver, |_| Ok(("latest", timestamp))).unwrap()
            );
            assert_eq!(receiver.len(), 1);
        }
        assert_eq!(in_flight, ("first", 100));
        assert_eq!(receiver.recv().unwrap(), ("latest", 500));
    }
}
