//! Real-process assertions, including zombies (using procfs on Linux and ps on macOS).
#![allow(dead_code)]

#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    use rustix::{
        io::Errno,
        process::{test_kill_process, Pid},
    };
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    // kill(pid, 0) sends no signal. A permission error still proves the process exists.
    match test_kill_process(pid) {
        Err(Errno::SRCH) => false,
        Ok(()) | Err(Errno::PERM) => {
            // Linux containers do not always reap orphaned zombies promptly. A zombie
            // cannot execute code and must not be mistaken for a surviving worker.
            #[cfg(target_os = "linux")]
            if let Ok(stat) =
                std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero()))
            {
                if stat
                    .rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
                {
                    return false;
                }
            }
            #[cfg(target_os = "macos")]
            {
                let output = std::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &pid.as_raw_nonzero().to_string()])
                    .output()
                    .expect("query process state with ps");
                let state = String::from_utf8_lossy(&output.stdout);
                // A process that disappeared between kill(0) and ps is also dead.
                if state.trim().is_empty() || state.trim_start().starts_with('Z') {
                    return false;
                }
                assert!(output.status.success(), "ps failed: {output:?}");
            }
            true
        }
        Err(error) => panic!("cannot query process {pid:?}: {error}"),
    }
}

#[cfg(unix)]
pub fn process_group(pid: u32) -> u32 {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .expect("valid process id");
    rustix::process::getpgid(Some(pid))
        .expect("process group")
        .as_raw_nonzero()
        .get() as u32
}
