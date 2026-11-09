#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
use std::process::{Child, Command};

/// Applies to this process only; call before creating native inference pools.
pub fn configure_inference_process(background: bool) -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::*;
        SetPriorityClass(
            GetCurrentProcess(),
            if background {
                BELOW_NORMAL_PRIORITY_CLASS
            } else {
                NORMAL_PRIORITY_CLASS
            },
        )
        .context("setting inference process CPU priority")?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = background;
        anyhow::bail!("Inference process scheduling requires Windows")
    }
}
#[cfg(windows)]
/// BelowNormal + Highest has the same base priority as Normal + Normal.
pub(crate) fn restore_capture_priority() -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::*;
        if GetPriorityClass(GetCurrentProcess()) == BELOW_NORMAL_PRIORITY_CLASS.0 {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST)
                .context("restoring audio capture effective CPU priority")?;
        }
    }
    Ok(())
}

/// A kill-on-close job owns the worker and every descendant (including FFmpeg).
/// The worker must wait for its configuration before starting descendants.
pub struct SessionProcess {
    pub child: Child,
    #[cfg(windows)]
    job: windows::Win32::Foundation::HANDLE,
}

impl SessionProcess {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::{io::AsRawHandle, process::CommandExt};
            use windows::Win32::{
                Foundation::CloseHandle,
                System::{JobObjects::*, Threading::CREATE_NO_WINDOW},
            };
            unsafe {
                let job = CreateJobObjectW(None, None).context("creating session process job")?;
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if let Err(error) = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    std::mem::size_of_val(&limits) as u32,
                ) {
                    let _ = CloseHandle(job);
                    return Err(error).context("configuring session process job");
                }
                command.creation_flags(CREATE_NO_WINDOW.0);
                let mut child = match command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = CloseHandle(job);
                        return Err(error).context("launching session worker");
                    }
                };
                if let Err(error) = AssignProcessToJobObject(
                    job,
                    windows::Win32::Foundation::HANDLE(child.as_raw_handle()),
                ) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = CloseHandle(job);
                    return Err(error).context("assigning session worker to kill-on-close job");
                }
                Ok(Self { child, job })
            }
        }
        #[cfg(not(windows))]
        {
            let _ = command;
            anyhow::bail!("Isolated session process ownership requires Windows")
        }
    }
}

impl Drop for SessionProcess {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.job);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
