use crate::{deliver, monotonic_ms, pause, status};
use anyhow::{Context, Result, ensure};
use crossbeam_channel::Sender;
use cs_core::{CaptureDiagnostics, CaptureEvent};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

fn open(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1 | 2 | 4);
    }
    options.open(path)
}

pub(crate) fn follow(
    path: &Path,
    tx: &Sender<CaptureEvent>,
    stop: &AtomicBool,
    diagnostics: &CaptureDiagnostics,
    ready: &Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let mut current: Option<(File, same_file::Handle)> = None;
    let mut position = 0;
    let mut pending = Vec::new();
    let mut anchor = Vec::new();
    let mut buffer = [0u8; 16384];
    let mut reported_missing = false;
    let mut attached = false;
    while !stop.load(Ordering::Relaxed) {
        let identity = match same_file::Handle::from_path(path) {
            Ok(identity) => {
                reported_missing = false;
                identity
            }
            Err(error) => {
                if !reported_missing {
                    status(
                        tx,
                        diagnostics,
                        "console",
                        format!("Waiting for {}: {error}", path.display()),
                        true,
                    );
                    reported_missing = true;
                }
                current = None;
                position = 0;
                pending.clear();
                anchor.clear();
                pause(stop, Duration::from_millis(100));
                continue;
            }
        };
        let replaced = current.as_ref().is_none_or(|(_, old)| *old != identity);
        if replaced {
            let mut file =
                open(path).with_context(|| format!("Open console {} read-only", path.display()))?;
            // Existing backlog is intentionally skipped when attaching; replacement files start at zero.
            if !attached {
                position = file.seek(SeekFrom::End(0))?;
                let length = position.min(64) as usize;
                anchor.resize(length, 0);
                file.seek(SeekFrom::Start(position - length as u64))?;
                if let Err(error) = file.read_exact(&mut anchor) {
                    if error.kind() != std::io::ErrorKind::UnexpectedEof {
                        return Err(error.into());
                    }
                    position = 0;
                    anchor.clear();
                }
            } else {
                position = 0;
                anchor.clear();
            }
            pending.clear();
            current = Some((file, identity));
            attached = true;
            let _ = ready.try_send(Ok(()));
            status(
                tx,
                diagnostics,
                "console",
                format!("Following {}", path.display()),
                false,
            );
        }
        let (file, _) = current.as_mut().context("Console file unavailable")?;
        let length = file.metadata()?.len();
        let mut changed = length < position;
        if !changed && !anchor.is_empty() {
            file.seek(SeekFrom::Start(position - anchor.len() as u64))?;
            let mut check = [0u8; 64];
            changed = file.read_exact(&mut check[..anchor.len()]).is_err()
                || check[..anchor.len()] != anchor;
        }
        if changed {
            position = 0;
            pending.clear();
            anchor.clear();
            status(
                tx,
                diagnostics,
                "console",
                "Console truncated/re-written; restarting at byte zero",
                false,
            );
        }
        file.seek(SeekFrom::Start(position))?;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            position += count as u64;
            // Retain the bytes actually read, not a second read racing truncation.
            update_anchor(&mut anchor, &buffer[..count]);
            pending.extend_from_slice(&buffer[..count]);
            let mut consumed = 0;
            for end in pending
                .iter()
                .enumerate()
                .filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
            {
                let bytes = &pending[consumed..=end];
                consumed = end + 1;
                match std::str::from_utf8(bytes) {
                    Ok(line) => {
                        if let Some(chat) = cs_core::parse_chat_line(
                            line.trim_start_matches('\u{feff}'),
                            monotonic_ms(),
                        ) && !deliver(tx, CaptureEvent::Chat(chat), diagnostics)
                        {
                            return Ok(());
                        }
                    }
                    Err(error) => status(
                        tx,
                        diagnostics,
                        "console",
                        format!("Invalid UTF-8 console line: {error}"),
                        true,
                    ),
                }
            }
            pending.drain(..consumed);
            ensure!(
                pending.len() <= 1024 * 1024,
                "Console line exceeded 1 MiB without newline"
            );
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
        pause(stop, Duration::from_millis(50));
    }
    Ok(())
}

fn update_anchor(anchor: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= 64 {
        anchor.clear();
        anchor.extend_from_slice(&bytes[bytes.len() - 64..]);
    } else {
        let discard = (anchor.len() + bytes.len()).saturating_sub(64);
        anchor.drain(..discard);
        anchor.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_anchor_keeps_last_bytes_without_reading_mutable_file() {
        let mut anchor = Vec::new();
        update_anchor(&mut anchor, &[1; 40]);
        update_anchor(&mut anchor, &[2; 40]);
        assert_eq!(&anchor[..24], &[1; 24]);
        assert_eq!(&anchor[24..], &[2; 40]);
        update_anchor(&mut anchor, &[3; 100]);
        assert_eq!(anchor, vec![3; 64]);
    }

    #[test]
    fn follows_split_utf8_truncation_and_rotation() {
        use std::{io::Write, sync::Arc};
        let directory = std::env::temp_dir().join(format!(
            "cs-tail-{}-{}",
            std::process::id(),
            crate::monotonic_ms()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("console.log");
        std::fs::write(&path, b"old backlog\n").unwrap();
        let (tx, rx) = crossbeam_channel::bounded(16);
        let stop = Arc::new(AtomicBool::new(false));
        let diagnostics = Arc::new(CaptureDiagnostics::default());
        let worker = crate::spawn_log_tail(path.clone(), tx, stop.clone(), diagnostics).unwrap();
        worker.wait_ready().unwrap();
        let receive_chat = || loop {
            if let CaptureEvent::Chat(chat) = rx.recv_timeout(Duration::from_secs(3)).unwrap() {
                break chat;
            }
        };
        let mut writer = OpenOptions::new().write(true).open(&path).unwrap();
        writer.seek(SeekFrom::End(0)).unwrap();
        writer.write_all(b"[CT] player: \xe7").unwrap();
        writer.flush().unwrap();
        writer.write_all(b"\x8c\xab\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(receive_chat().text, "猫");
        writer.set_len(0).unwrap();
        writer.seek(SeekFrom::Start(0)).unwrap();
        writer
            .write_all(b"[CT] player: after truncation\n")
            .unwrap();
        writer.flush().unwrap();
        assert_eq!(receive_chat().text, "after truncation");
        drop(writer);
        std::fs::rename(&path, directory.join("old.log")).unwrap();
        std::fs::write(&path, b"[CT] player: replacement\n").unwrap();
        assert_eq!(receive_chat().text, "replacement");
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
