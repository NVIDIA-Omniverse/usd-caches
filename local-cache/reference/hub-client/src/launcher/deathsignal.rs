// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub trait DeathSignal {
    fn death_signal(&mut self);
}

#[cfg(target_os = "linux")]
impl DeathSignal for std::process::Command {
    fn death_signal(&mut self) {
        use std::io;
        use std::os::unix::process::CommandExt;
        let ppid_before_fork = std::process::id();
        unsafe {
            self.pre_exec(move || {
                use libc::{prctl, PR_SET_PDEATHSIG, SIGTERM};
                let result = prctl(PR_SET_PDEATHSIG, SIGTERM, 0, 0, 0);
                assert_eq!(result, 0);

                // test in case the original parent exited just before the prctl() call
                if std::os::unix::process::parent_id() != ppid_before_fork {
                    return Err(io::Error::other("parent pid changed after fork!"));
                }

                Ok(())
            });
        }
    }
}

#[cfg(target_os = "windows")]
impl DeathSignal for std::process::Command {
    fn death_signal(&mut self) {
        use std::ffi::c_void;
        use std::io;
        use std::mem;
        use std::process;
        use tracing::{debug, error};
        use windows::{
            core::PCWSTR,
            Win32::Foundation::{CloseHandle, BOOL},
            Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Win32::System::Threading::GetCurrentProcess,
        };

        unsafe {
            let mut in_job: BOOL = BOOL::default();
            let result = IsProcessInJob(GetCurrentProcess(), None, &mut in_job);
            debug!(id = process::id(), ?in_job, ?result, last_error = ?io::Error::last_os_error(), "IsProcessInJob");
            assert!(result.is_ok(), "IsProcessInJob: {}", io::Error::last_os_error());

            let result = CreateJobObjectW(None, PCWSTR::null());
            debug!(id = process::id(), ?in_job, ?result, last_error = ?io::Error::last_os_error(), "CreateJobObjectW");
            assert!(
                result.is_ok(),
                "CreateJobObjectW: {:?} {:?}",
                result,
                io::Error::last_os_error()
            );
            let handle = result.unwrap();

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let result = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                mem::size_of_val(&info) as u32,
            );
            debug!(id = process::id(), ?handle, ?in_job, ?result, last_error = ?io::Error::last_os_error(), "SetInformationJobObject");
            assert!(
                result.is_ok(),
                "SetInformationJobObject: {:?} {:?}",
                result,
                io::Error::last_os_error()
            );

            let result = AssignProcessToJobObject(handle, GetCurrentProcess());
            debug!(id = process::id(), ?handle, ?in_job, ?result, last_error = ?io::Error::last_os_error(), "AssignProcessToJobObject");

            if result.is_err() {
                error!(id = process::id(), ?handle, ?in_job, ?result, last_error = ?io::Error::last_os_error(), "AssignProcessToJobObject");
                let result = CloseHandle(handle);
                debug!(id = process::id(), ?handle, ?in_job, ?result, last_error = ?io::Error::last_os_error(), "CloseHandle");
            }
        }
    }
}
