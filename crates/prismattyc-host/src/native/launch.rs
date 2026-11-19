// SPDX-License-Identifier: MIT
//! Deliberate native inheritance belongs to the same owner that launches the PTY.

use super::{
    authority::{DesktopNative, Geometry},
    profile,
    transport::AVAILABILITY,
};
use prismattyc_emulator::PtySession;
use std::{collections::BTreeMap, io, path::Path, time::Instant};

const CHILD_FD: i32 = 3;

pub struct LaunchedPty {
    pub session: PtySession,
    pub native: Option<BoundNative>,
}

pub struct BoundNative {
    pub authority: DesktopNative,
    identity: ChildIdentity,
}
impl BoundNative {
    pub fn identity(&self) -> &ChildIdentity {
        &self.identity
    }
}

pub struct ChildIdentity {
    pid: u32,
    born: [u64; 2],
}
impl ChildIdentity {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn birth(&self) -> [u64; 2] {
        self.born
    }
}

/// Only direct desktop-local launches pass a geometry. Other routes pass None.
/// Failure to prepare or bind native authority preserves the ordinary PTY child.
pub fn spawn(
    program: &str,
    arguments: &[String],
    cwd: Option<&Path>,
    environment: &BTreeMap<String, String>,
    size: portable_pty::PtySize,
    geometry: Option<Geometry>,
) -> anyhow::Result<LaunchedPty> {
    let geometry = geometry.filter(|geometry| {
        geometry.matches_grid(u32::from(size.cols), u32::from(size.rows))
            && AVAILABILITY.available()
            && !profile::dialects().is_empty()
    });
    let prepared = geometry.and_then(|geometry| {
        prismattyc_process::fd::socket_pair()
            .ok()
            .map(|pair| (geometry, pair))
    });
    let mut environment = environment.clone();
    environment.remove("PRISMATTYC_NATIVE_FD");
    let Some((geometry, (host, child))) = prepared else {
        let session = PtySession::spawn_config(program, arguments, cwd, &environment, size)?;
        return Ok(LaunchedPty {
            session,
            native: None,
        });
    };
    environment.insert("PRISMATTYC_NATIVE_FD".into(), CHILD_FD.to_string());
    let session = PtySession::spawn_config_with_fds(
        program,
        arguments,
        cwd,
        &environment,
        size,
        vec![(child, CHILD_FD)],
    )?;
    // This newly returned session has not been reaped or exposed to another
    // owner. Its PID cannot be reused while its child remains unreaped.
    let identity = session
        .process_id()
        .and_then(|pid| child_identity(pid).ok());
    let native = identity.and_then(|identity| {
        DesktopNative::new(host.into(), geometry, Instant::now())
            .ok()
            .map(|authority| BoundNative {
                authority,
                identity,
            })
    });
    Ok(LaunchedPty { session, native })
}

fn child_identity(pid: u32) -> io::Result<ChildIdentity> {
    let pid_native = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "native child PID"))?;
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as i32;
        let count = unsafe {
            libc::proc_pidinfo(
                pid_native,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if count != size
            || info.pbi_pid != pid
            || info.pbi_ppid != std::process::id()
            || info.pbi_uid != unsafe { libc::geteuid() }
            || info.pbi_status == libc::SZOMB
            || info.pbi_status == 0
            || info.pbi_start_tvsec == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native launch child identity",
            ));
        }
        Ok(ChildIdentity {
            pid,
            born: [info.pbi_start_tvsec, info.pbi_start_tvusec],
        })
    }
    #[cfg(target_os = "linux")]
    {
        let _ = pid_native;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let field = |name: &str| status.lines().find_map(|line| line.strip_prefix(name));
        let parse = |text: Option<&str>| text.and_then(|text| text.trim().parse::<u32>().ok());
        let uid = field("Uid:")
            .and_then(|text| text.split_whitespace().nth(1))
            .and_then(|text| text.parse::<u32>().ok());
        if parse(field("Pid:")) != Some(pid)
            || parse(field("PPid:")) != Some(std::process::id())
            || uid != Some(unsafe { libc::geteuid() })
            || field("State:").is_none_or(|state| state.trim_start().starts_with('Z'))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native launch child identity",
            ));
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let born = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .and_then(|ticks| ticks.parse::<u64>().ok())
            .filter(|ticks| *ticks != 0)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "native child birth identity")
            })?;
        Ok(ChildIdentity {
            pid,
            born: [born, 0],
        })
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
