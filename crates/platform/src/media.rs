//! Shared, bounded FFmpeg pipe formats for replay and benchmarking.
use anyhow::{Result, ensure};
use cs_core::NormalizedRect;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

pub const SAMPLE_RATE: u32 = 48_000;
const MAX_CHUNK_SAMPLES: usize = SAMPLE_RATE as usize * 60;
const MAX_FRAME_PIXELS: usize = 16_777_216;

#[derive(Clone, Copy, Debug)]
pub struct MediaClip {
    start_seconds: f64,
    duration_seconds: Option<f64>,
}
impl MediaClip {
    pub fn new(start_seconds: f64, duration_seconds: Option<f64>) -> Result<Self> {
        ensure!(
            start_seconds.is_finite() && start_seconds >= 0.0,
            "Clip start must be finite and nonnegative"
        );
        ensure!(
            duration_seconds.is_none_or(|d| d.is_finite() && d > 0.0),
            "Clip duration must be finite and positive"
        );
        Ok(Self {
            start_seconds,
            duration_seconds,
        })
    }
}

fn input_command(executable: &Path, input: &Path, clip: MediaClip, paced: bool) -> Command {
    let mut command = Command::new(executable);
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
    if paced {
        command.arg("-re");
    }
    // Input seeking without copyts normalizes the selected clip to zero while
    // retaining gaps inside the clip. Both consumers use this exact policy.
    command
        .arg("-ss")
        .arg(clip.start_seconds.to_string())
        .arg("-i")
        .arg(input);
    if let Some(duration) = clip.duration_seconds {
        command.arg("-t").arg(duration.to_string());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

pub fn pcm_command(executable: &Path, input: &Path, clip: MediaClip, paced: bool) -> Command {
    let mut command = input_command(executable, input, clip, paced);
    command.args([
        "-map",
        "0:a:0",
        "-vn",
        "-af",
        "aresample=async=1:first_pts=0",
        "-ac",
        "1",
        "-ar",
        "48000",
        "-c:a",
        "pcm_f32le",
        "-f",
        "f32le",
        "pipe:1",
    ]);
    command
}

pub fn frame_command(
    executable: &Path,
    input: &Path,
    clip: MediaClip,
    paced: bool,
    interval_ms: u64,
    roi: NormalizedRect,
) -> Result<Command> {
    ensure!(interval_ms > 0, "Frame interval must be positive");
    ensure!(
        [roi.x, roi.y, roi.width, roi.height]
            .iter()
            .all(|v| v.is_finite()),
        "Frame ROI must be finite"
    );
    ensure!(
        roi.x >= 0.0
            && roi.y >= 0.0
            && roi.width > 0.0
            && roi.height > 0.0
            && roi.x + roi.width <= 1.0
            && roi.y + roi.height <= 1.0,
        "Frame ROI must fit the image"
    );
    let mut command = input_command(executable, input, clip, paced);
    command
        .args(["-map", "0:v:0", "-an", "-vf"])
        .arg(format!(
            "fps=1000/{interval_ms}:start_time=0,crop=iw*{}:ih*{}:iw*{}:ih*{}",
            roi.width, roi.height, roi.x, roi.y
        ))
        .args([
            "-pix_fmt",
            "rgb24",
            "-f",
            "image2pipe",
            "-vcodec",
            "ppm",
            "pipe:1",
        ]);
    Ok(command)
}

pub struct PcmChunk {
    pub timestamp_ms: u64,
    pub samples: Vec<f32>,
}

pub struct PcmReader<R> {
    reader: R,
    bytes: Vec<u8>,
    valid_bytes: usize,
    sample_count: u64,
}
impl<R: Read> PcmReader<R> {
    pub fn new(reader: R, chunk_samples: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_CHUNK_SAMPLES).contains(&chunk_samples),
            "PCM chunk must contain 1..=2880000 samples"
        );
        Ok(Self {
            reader,
            bytes: vec![0; chunk_samples * 4],
            valid_bytes: 0,
            sample_count: 0,
        })
    }
    /// Exact little-endian bytes from the last chunk, suitable for incremental hashing.
    pub fn raw_bytes(&self) -> &[u8] {
        &self.bytes[..self.valid_bytes]
    }
    pub fn read_chunk(&mut self) -> io::Result<Option<PcmChunk>> {
        self.valid_bytes = 0;
        while self.valid_bytes < self.bytes.len() {
            match self.reader.read(&mut self.bytes[self.valid_bytes..]) {
                Ok(0) => break,
                Ok(n) => self.valid_bytes += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if self.valid_bytes == 0 {
            return Ok(None);
        }
        if !self.valid_bytes.is_multiple_of(4) {
            return Err(invalid("Truncated float PCM sample"));
        }
        let timestamp_ms = self.sample_count / u64::from(SAMPLE_RATE) * 1000
            + self.sample_count % u64::from(SAMPLE_RATE) * 1000 / u64::from(SAMPLE_RATE);
        let samples: Vec<f32> = self
            .raw_bytes()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        self.sample_count = self
            .sample_count
            .checked_add(samples.len() as u64)
            .ok_or_else(|| invalid("PCM timestamp overflow"))?;
        Ok(Some(PcmChunk {
            timestamp_ms,
            samples,
        }))
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub struct PpmReader<R> {
    reader: BufReader<R>,
    rgb: Vec<u8>,
    interval_ms: u64,
    index: u64,
}
impl<R: Read> PpmReader<R> {
    pub fn new(reader: R, interval_ms: u64) -> Result<Self> {
        ensure!(interval_ms > 0, "Frame interval must be positive");
        Ok(Self {
            reader: BufReader::new(reader),
            rgb: Vec::new(),
            interval_ms,
            index: 0,
        })
    }
    /// None means EOF before a new header; any partial header or raster is an error.
    pub fn read_frame(&mut self, frame: &mut image::RgbaImage) -> io::Result<Option<u64>> {
        if self.reader.fill_buf()?.is_empty() {
            return Ok(None);
        }
        let mut budget = 4096;
        if ppm_token(&mut self.reader, &mut budget)? != "P6" {
            return Err(invalid("Expected P6 PPM frame"));
        }
        let width = ppm_token(&mut self.reader, &mut budget)?
            .parse::<u32>()
            .map_err(|_| invalid("Invalid PPM width"))?;
        let height = ppm_token(&mut self.reader, &mut budget)?
            .parse::<u32>()
            .map_err(|_| invalid("Invalid PPM height"))?;
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| invalid("PPM dimensions overflow"))?;
        if width == 0 || height == 0 || width > 16384 || height > 16384 || pixels > MAX_FRAME_PIXELS
        {
            return Err(invalid("PPM frame exceeds dimension limit"));
        }
        if ppm_token(&mut self.reader, &mut budget)? != "255" {
            return Err(invalid("Unsupported PPM depth"));
        }
        self.rgb.resize(pixels * 3, 0);
        self.reader.read_exact(&mut self.rgb)?;
        if frame.dimensions() != (width, height) {
            *frame = image::RgbaImage::new(width, height);
        }
        for (target, source) in frame.pixels_mut().zip(self.rgb.as_chunks::<3>().0) {
            *target = image::Rgba([source[0], source[1], source[2], 255]);
        }
        let timestamp = self
            .index
            .checked_mul(self.interval_ms)
            .ok_or_else(|| invalid("Frame timestamp overflow"))?;
        self.index = self
            .index
            .checked_add(1)
            .ok_or_else(|| invalid("Frame index overflow"))?;
        Ok(Some(timestamp))
    }
}
fn ppm_token(reader: &mut impl BufRead, budget: &mut usize) -> io::Result<String> {
    let mut token = Vec::with_capacity(16);
    let mut comment = false;
    loop {
        if *budget == 0 {
            return Err(invalid("Oversized PPM header"));
        }
        *budget -= 1;
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        let b = byte[0];
        if comment {
            if b == b'\n' {
                comment = false;
            }
            continue;
        }
        if b == b'#' && token.is_empty() {
            comment = true;
            continue;
        }
        if b.is_ascii_whitespace() {
            if token.is_empty() {
                continue;
            }
            if b == b'\r' && reader.fill_buf()?.first() == Some(&b'\n') {
                reader.consume(1);
            }
            return String::from_utf8(token).map_err(|_| invalid("Non-ASCII PPM token"));
        }
        if token.len() >= 32 {
            return Err(invalid("Oversized PPM token"));
        }
        token.push(b);
    }
}

/// Owns and reaps a child even if its pipe reader is blocked during cancellation.
pub struct CancellableChild {
    child: Arc<Mutex<Child>>,
    pub stdout: Option<ChildStdout>,
    done: Arc<AtomicBool>,
    monitor: Option<thread::JoinHandle<()>>,
}
impl CancellableChild {
    pub fn new(mut child: Child, stop: &Arc<AtomicBool>) -> Self {
        let stdout = child.stdout.take();
        let child = Arc::new(Mutex::new(child));
        let done = Arc::new(AtomicBool::new(false));
        let watched_child = child.clone();
        let watched_done = done.clone();
        let stop = stop.clone();
        let monitor = thread::spawn(move || {
            while !watched_done.load(Ordering::Acquire) {
                if stop.load(Ordering::Relaxed) {
                    let _ = watched_child
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .kill();
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
        });
        Self {
            child,
            stdout,
            done,
            monitor: Some(monitor),
        }
    }
    pub fn finish(&mut self, kill: bool) -> Result<ExitStatus> {
        if kill {
            let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
        }
        let result = loop {
            match self
                .child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .try_wait()
            {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(error) => break Err(error.into()),
            }
        };
        self.done.store(true, Ordering::Release);
        if let Some(monitor) = self.monitor.take() {
            monitor
                .join()
                .map_err(|_| anyhow::anyhow!("FFmpeg cancellation monitor panicked"))?;
        }
        result
    }
}
impl Drop for CancellableChild {
    fn drop(&mut self) {
        if !self.done.load(Ordering::Acquire) {
            let _ = self.finish(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn pcm_chunks_keep_exact_bytes_and_sequential_timestamps() {
        let samples: Vec<f32> = (0..481).map(|i| i as f32 / 1000.0).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut reader = PcmReader::new(Cursor::new(&bytes), 480).unwrap();
        let first = reader.read_chunk().unwrap().unwrap();
        assert_eq!(first.timestamp_ms, 0);
        assert_eq!(first.samples, samples[..480]);
        assert_eq!(reader.raw_bytes(), &bytes[..1920]);
        let last = reader.read_chunk().unwrap().unwrap();
        assert_eq!(last.timestamp_ms, 10);
        assert_eq!(last.samples, samples[480..]);
        assert!(reader.read_chunk().unwrap().is_none());
    }

    #[test]
    fn pcm_rejects_truncated_sample_and_invalid_bounds() {
        assert!(PcmReader::new(Cursor::new([0; 3]), 0).is_err());
        assert!(PcmReader::new(Cursor::new([0; 3]), MAX_CHUNK_SAMPLES + 1).is_err());
        assert!(
            PcmReader::new(Cursor::new([0; 3]), 1)
                .unwrap()
                .read_chunk()
                .is_err()
        );
        assert!(MediaClip::new(f64::NAN, None).is_err());
        assert!(MediaClip::new(0.0, Some(0.0)).is_err());
    }

    #[test]
    fn ppm_preserves_binary_whitespace_and_frame_timing() {
        let mut data = b"P6\n# dimensions\n1 1\n255\n".to_vec();
        data.extend([10, 32, 35]);
        data.extend(b"P6\r\n1 1\r\n255\r\n");
        data.extend([0, 128, 255]);
        let mut reader = PpmReader::new(Cursor::new(data), 125).unwrap();
        let mut frame = image::RgbaImage::new(0, 0);
        assert_eq!(reader.read_frame(&mut frame).unwrap(), Some(0));
        assert_eq!(frame.get_pixel(0, 0).0, [10, 32, 35, 255]);
        assert_eq!(reader.read_frame(&mut frame).unwrap(), Some(125));
        assert_eq!(frame.get_pixel(0, 0).0, [0, 128, 255, 255]);
        assert_eq!(reader.read_frame(&mut frame).unwrap(), None);
    }

    #[test]
    fn ppm_distinguishes_clean_eof_from_truncation_and_limits() {
        for bytes in [
            b"P6\n1".as_slice(),
            b"P6\n1 1\n255\n\0\0",
            b"P6\n16384 16384\n255\n",
            b"P6\n1 1\n65535\n",
        ] {
            let mut reader = PpmReader::new(Cursor::new(bytes), 125).unwrap();
            assert!(reader.read_frame(&mut image::RgbaImage::new(0, 0)).is_err());
        }
        let mut reader = PpmReader::new(Cursor::new([]), 125).unwrap();
        assert!(
            reader
                .read_frame(&mut image::RgbaImage::new(0, 0))
                .unwrap()
                .is_none()
        );
    }
}
