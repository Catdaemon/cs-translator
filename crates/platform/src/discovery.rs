use crate::GameSource;
use anyhow::{Context, Result};
use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};
use windows::{
    Win32::{
        Foundation::{CloseHandle, RECT},
        Globalization::GetUserDefaultLocaleName,
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::GetClientRect,
    },
    core::PWSTR,
};

/// Identify the real executable, never a window merely titled like CS2.
/// PROCESS_QUERY_LIMITED_INFORMATION reads process metadata, not game memory.
pub fn discover_cs2_source() -> Result<Option<GameSource>> {
    let mut path = [0u16; 32768];
    let mut best: Option<(i64, GameSource)> = None;
    for window in crate::list_windows().context("Discover CS2 windows")? {
        let Ok(process) =
            (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, window.process_id) })
        else {
            continue;
        };
        let mut size = path.len() as u32;
        let queried = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut size,
            )
        };
        let _ = unsafe { CloseHandle(process) };
        if queried.is_err() {
            continue;
        }
        let text = &path[..size as usize];
        let name = text
            .rsplit(|c| *c == b'\\' as u16 || *c == b'/' as u16)
            .next()
            .unwrap_or_default();
        if name.len() != 7
            || !name
                .iter()
                .zip(b"cs2.exe")
                .all(|(a, b)| *a <= 127 && (*a as u8).eq_ignore_ascii_case(b))
        {
            continue;
        }
        let mut rect = RECT::default();
        if unsafe {
            GetClientRect(
                windows::Win32::Foundation::HWND(window.handle as *mut _),
                &mut rect,
            )
        }
        .is_err()
        {
            continue;
        }
        let area =
            i64::from((rect.right - rect.left).max(0)) * i64::from((rect.bottom - rect.top).max(0));
        if area == 0 || best.as_ref().is_some_and(|(current, _)| *current >= area) {
            continue;
        }
        let executable = PathBuf::from(OsString::from_wide(text));
        let game = executable
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent());
        let console_log = game.and_then(newest_console_log);
        best = Some((
            area,
            GameSource {
                process_id: window.process_id,
                window: window.handle,
                console_log,
            },
        ));
    }
    Ok(best.map(|(_, source)| source))
}

pub fn preferred_language() -> Option<String> {
    let mut locale = [0u16; 85];
    let length = unsafe { GetUserDefaultLocaleName(&mut locale) };
    if length <= 1 {
        return None;
    }
    let name = String::from_utf16_lossy(&locale[..length as usize - 1]);
    Some(name.split('-').next()?.to_ascii_lowercase())
}

fn newest_console_log(root: &std::path::Path) -> Option<PathBuf> {
    [
        root.join("csgo/console.log"),
        root.join("csgo/logs/console.log"),
    ]
    .into_iter()
    .filter_map(|path| {
        let metadata = path.metadata().ok()?;
        metadata.is_file().then(|| (metadata.modified().ok(), path))
    })
    .max_by_key(|(modified, _)| *modified)
    .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_selection_tracks_newest_writer_not_path_order() {
        use std::{
            fs::{File, FileTimes},
            time::{Duration, SystemTime},
        };
        let directory = std::env::temp_dir().join(format!(
            "cs-discovery-{}-{}",
            std::process::id(),
            crate::monotonic_ms()
        ));
        std::fs::create_dir_all(directory.join("csgo/logs")).unwrap();
        let first = directory.join("csgo/console.log");
        let second = directory.join("csgo/logs/console.log");
        std::fs::write(&first, b"old").unwrap();
        std::fs::write(&second, b"active").unwrap();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&first)
            .unwrap()
            .set_times(FileTimes::new().set_modified(base))
            .unwrap();
        File::options()
            .write(true)
            .open(&second)
            .unwrap()
            .set_times(FileTimes::new().set_modified(base + Duration::from_secs(10)))
            .unwrap();
        assert_eq!(newest_console_log(&directory), Some(second));
        File::options()
            .write(true)
            .open(&first)
            .unwrap()
            .set_times(FileTimes::new().set_modified(base + Duration::from_secs(20)))
            .unwrap();
        assert_eq!(newest_console_log(&directory), Some(first));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
