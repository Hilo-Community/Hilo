//! EXPERIMENT ONLY: which part of the launch keeps a sandboxed `cmd` from starting a child.
#![cfg(windows)]

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;

use sandbox_windows::{spawn, JobLimits, LaunchConfig};

fn run(i: usize, flags: u32, program: &str, args: &[&str], work: &std::path::Path) -> String {
    std::env::set_var("COLORIDE_EXP", flags.to_string());
    let sys = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    for var in [
        "SystemRoot",
        "windir",
        "SystemDrive",
        "ComSpec",
        "PATHEXT",
        "ProgramData",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "PUBLIC",
    ] {
        if let Some(v) = std::env::var_os(var) {
            env.push((var.into(), v));
        }
    }
    env.push(("PATH".into(), sys.clone().into_os_string()));
    let config = LaunchConfig {
        program: sys.join(program),
        args: args.iter().map(OsString::from).collect(),
        cwd: work.to_path_buf(),
        env,
        writable_dirs: vec![work.to_path_buf()],
        journal_dir: work.parent().unwrap().join("journal"),
        limits: JobLimits::default(),
        container_name: format!("coloride-exp-{}-{i}", std::process::id()),
        stdin_piped: false,
    };
    match spawn(&config) {
        Err(e) => format!("spawn error: {e}"),
        Ok(mut s) => {
            let code = s.process.wait().unwrap();
            let (mut out, mut err) = (String::new(), String::new());
            let _ = s.stdout.read_to_string(&mut out);
            let _ = s.stderr.read_to_string(&mut err);
            format!("exit {code:#x} | out: {} | err: {}", out.trim(), err.trim())
        }
    }
}

#[test]
fn which_part_blocks_children() {
    let base = std::env::temp_dir().join(format!("coloride-exp-{}", std::process::id()));
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut report = String::new();
    let cases: &[(&str, &[&str])] = &[
        ("whoami.exe", &[]),
        ("cmd.exe", &["/d", "/c", "whoami"]),
        ("cmd.exe", &["/d", "/c", r"C:\Windows\System32\whoami.exe"]),
        (
            "cmd.exe",
            &["/d", "/c", r"C:\Windows\System32\cmd.exe /d /c echo nested"],
        ),
    ];
    let mut i = 0;
    for flags in [0u32, 1, 2, 4, 3, 7] {
        for (program, args) in cases {
            i += 1;
            let line = run(i, flags, program, args, &work);
            report.push_str(&format!("flags={flags} {program} {args:?} => {line}\n"));
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    panic!("EXPERIMENT REPORT\n{report}");
}
