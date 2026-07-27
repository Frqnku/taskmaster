use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Instant;

#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;

use config::model::Program;
use logger::LogEvent;

#[cfg(unix)]
const SIGKILL: i32 = 9;
#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(any(target_os = "linux", target_os = "android"))]
const SIGUSR1: i32 = 10;
#[cfg(any(target_os = "linux", target_os = "android"))]
const SIGUSR2: i32 = 12;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const SIGUSR1: i32 = 30;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const SIGUSR2: i32 = 31;
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
const SIGUSR1: i32 = 16;
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
const SIGUSR2: i32 = 17;
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos"
    ))
))]
const SIGUSR1: i32 = 10;
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos"
    ))
))]
const SIGUSR2: i32 = 12;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn umask(mask: u32) -> u32;
}

#[derive(Debug)]
pub enum ProcessError {
    Io(io::Error),
    Spawn(String),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{}", error),
            Self::Spawn(message) => write!(formatter, "{}", message),
        }
    }
}

impl std::error::Error for ProcessError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPoll {
    Running,
    Exited(Option<i32>),
    Stopped,
}

impl From<io::Error> for ProcessError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A single supervised child process.
#[derive(Debug)]
pub struct ProcessInstance {
    pub id: usize,
    pub name: String,
    pub child: Option<Child>,
    pub stdout_path: Option<PathBuf>,
    pub stderr_path: Option<PathBuf>,
    pub started_at: Option<Instant>,
    pub stopsignal: Option<String>,
    pub stoptime: u64,
}

impl ProcessInstance {
    /// Spawn a command in its own process group.
    pub fn spawn(name: &str, id: usize, program: &Program) -> Result<Self, ProcessError> {
        let stdout_path = program.stdout.as_ref().map(PathBuf::from);
        let stderr_path = program.stderr.as_ref().map(PathBuf::from);
        let stdout = output_target(stdout_path.as_ref())?;
        let stderr = output_target(stderr_path.as_ref())?;

        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            command.arg("-c").arg(&program.cmd);
            command
        };

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd.exe");
            command.arg("/S").arg("/C").arg(&program.cmd);
            command
        };

        #[cfg(not(any(unix, windows)))]
        let mut command = {
            let mut parts = program.cmd.split_whitespace();
            let executable = parts
                .next()
                .ok_or_else(|| ProcessError::Spawn("empty cmd".to_string()))?;
            let mut command = Command::new(executable);
            command.args(parts);
            command
        };

        if let Some(directory) = &program.workingdir {
            command.current_dir(directory);
        }
        if let Some(environment) = &program.env {
            command.envs(environment);
        }
        command.stdout(stdout).stderr(stderr);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;

            let configured_umask = program.umask;
            unsafe {
                command.pre_exec(move || {
                    if setpgid(0, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if let Some(mask) = configured_umask {
                        umask(mask);
                    }
                    Ok(())
                });
            }
        }

        let child = command.spawn()?;
        let pid = child.id();
        let _ = logger::log(LogEvent::Start {
            program: name.to_string(),
            id,
            pid: Some(pid),
        });

        Ok(Self {
            id,
            name: name.to_string(),
            child: Some(child),
            stdout_path,
            stderr_path,
            started_at: Some(Instant::now()),
            stopsignal: program.stopsignal.clone(),
            stoptime: program.stoptime,
        })
    }

    /// Stop the whole process group gracefully, then force-kill survivors.
    pub fn stop(&mut self) -> Result<(), ProcessError> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let pid = child.id();
        let _ = pid;

        #[cfg(unix)]
        {
            let signal_name = self.stopsignal.as_deref().unwrap_or("TERM");
            let signal = map_signal_name(signal_name).map_err(|_| {
                ProcessError::Spawn(format!("invalid stop signal '{}'", signal_name))
            })?;

            let _ = signal_process_group(pid, signal);
            let timeout = Duration::from_secs(self.stoptime);
            let started = Instant::now();

            loop {
                let _ = child.try_wait();
                if !process_group_exists(pid) || started.elapsed() >= timeout {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }

            if process_group_exists(pid) {
                let _ = signal_process_group(pid, SIGKILL);
            }
            let _ = child.wait();
        }

        #[cfg(not(unix))]
        {
            let _ = child.kill();
            let _ = child.wait();
        }

        let _ = logger::log(LogEvent::Stop {
            program: self.name.clone(),
            id: self.id,
        });
        Ok(())
    }

    /// Check whether the child leader is still alive.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.as_mut().map(Child::try_wait), Some(Ok(None)))
    }

    /// Poll the child leader without blocking.
    pub fn poll(&mut self) -> Result<ProcessPoll, ProcessError> {
        match self.child.as_mut() {
            Some(child) => match child.try_wait()? {
                Some(status) => Ok(ProcessPoll::Exited(exit_code(status))),
                None => Ok(ProcessPoll::Running),
            },
            None => Ok(ProcessPoll::Stopped),
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
}

fn output_target(path: Option<&PathBuf>) -> Result<Stdio, ProcessError> {
    match path {
        Some(path) => Ok(Stdio::from(
            OpenOptions::new().create(true).append(true).open(path)?,
        )),
        None => Ok(Stdio::from(
            OpenOptions::new().write(true).open(null_device_path())?,
        )),
    }
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code()
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: i32) -> io::Result<()> {
    let process_group = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid does not fit i32"))?;
    let result = unsafe { kill(-process_group, signal) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn process_group_exists(pid: u32) -> bool {
    let Ok(process_group) = i32::try_from(pid) else {
        return false;
    };
    if unsafe { kill(-process_group, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(1)
}

#[cfg(unix)]
fn map_signal_name(name: &str) -> Result<i32, ()> {
    if let Ok(number) = name.parse::<i32>() {
        return if number > 0 { Ok(number) } else { Err(()) };
    }
    match name.trim().to_uppercase().as_str() {
        "SIGHUP" | "HUP" => Ok(1),
        "SIGINT" | "INT" => Ok(2),
        "SIGQUIT" | "QUIT" => Ok(3),
        "SIGKILL" | "KILL" => Ok(SIGKILL),
        "SIGUSR1" | "USR1" => Ok(SIGUSR1),
        "SIGTERM" | "TERM" => Ok(SIGTERM),
        "SIGUSR2" | "USR2" => Ok(SIGUSR2),
        _ => Err(()),
    }
}

fn null_device_path() -> &'static str {
    #[cfg(unix)]
    {
        "/dev/null"
    }

    #[cfg(windows)]
    {
        // NUL is the Windows null device. It is equivalent to /dev/null.
        "NUL"
    }

    #[cfg(not(any(unix, windows)))]
    {
        "/dev/null"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn unique_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        path.push(format!(
            "taskmaster-{}-{}-{}",
            name,
            std::process::id(),
            unique
        ));
        path
    }

    #[cfg(unix)]
    fn write_executable_script(name: &str, body: &str) -> PathBuf {
        let path = unique_path(name);
        fs::write(&path, format!("#!/bin/sh\n{}\n", body)).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
        path
    }

    #[cfg(windows)]
    fn write_executable_script(name: &str, body: &str) -> PathBuf {
        let mut path = unique_path(name);
        path.set_extension("cmd");
        fs::write(&path, format!("@echo off\r\n{}\r\n", body)).expect("write script");
        path
    }

    fn command_for(path: &Path) -> String {
        path.display().to_string()
    }

    fn program_for(script: &Path) -> Program {
        Program {
            cmd: command_for(script),
            ..Program::default()
        }
    }

    #[test]
    fn spawns_and_reports_running_child() {
        #[cfg(unix)]
        let script = write_executable_script("instance-running", "sleep 1");
        #[cfg(windows)]
        let script = write_executable_script("instance-running", "ping -n 3 127.0.0.1 >NUL");
        let program = program_for(&script);

        let mut instance = ProcessInstance::spawn("web", 0, &program).expect("spawn child");
        assert!(instance.pid().is_some());
        assert!(instance.is_running());

        instance.stop().expect("stop child");
        assert!(instance.child.is_none());
        let _ = fs::remove_file(script);
    }

    #[test]
    fn redirects_stdout_and_stderr_to_files() {
        #[cfg(unix)]
        let script = write_executable_script(
            "instance-redirect",
            "echo hello-out\necho hello-err 1>&2\nexit 0",
        );
        #[cfg(windows)]
        let script = write_executable_script(
            "instance-redirect",
            "echo hello-out\necho hello-err 1>&2\nexit /b 0",
        );
        let mut program = program_for(&script);
        let stdout_path = unique_path("stdout.log");
        let stderr_path = unique_path("stderr.log");
        program.stdout = Some(stdout_path.display().to_string());
        program.stderr = Some(stderr_path.display().to_string());

        let mut instance = ProcessInstance::spawn("web", 1, &program).expect("spawn child");
        std::thread::sleep(Duration::from_millis(150));
        let _ = instance.poll().expect("poll child");
        instance.stop().expect("stop child");

        assert!(!fs::read(&stdout_path).expect("read stdout").is_empty());
        assert!(!fs::read(&stderr_path).expect("read stderr").is_empty());
        let _ = fs::remove_file(script);
        let _ = fs::remove_file(stdout_path);
        let _ = fs::remove_file(stderr_path);
    }

    #[test]
    fn shell_builtins_are_valid_commands() {
        let program = Program {
            cmd: "exit 0".to_string(),
            starttime: 0,
            ..Program::default()
        };
        let mut instance = ProcessInstance::spawn("oneshot", 0, &program).expect("spawn builtin");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(instance.poll().expect("poll"), ProcessPoll::Exited(Some(0)));
    }

    #[cfg(unix)]
    #[test]
    fn stop_sends_term_and_kills_the_process_group() {
        let marker = unique_path("term-marker");
        let child_pid_path = unique_path("child-pid");
        let body = format!(
            r#"trap 'echo term > "{}"; exit 0' TERM
sleep 30 &
echo $! > "{}"
wait"#,
            marker.display(),
            child_pid_path.display()
        );
        let script = write_executable_script("instance-process-group", &body);
        let mut program = program_for(&script);
        program.stoptime = 1;
        let mut instance = ProcessInstance::spawn("tree", 0, &program).expect("spawn tree");

        for _ in 0..50 {
            if child_pid_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let child_pid: i32 = fs::read_to_string(&child_pid_path)
            .expect("child pid file")
            .trim()
            .parse()
            .expect("child pid");

        instance.stop().expect("stop process group");
        assert!(marker.exists(), "the process did not receive SIGTERM");
        for _ in 0..50 {
            if unsafe { kill(child_pid, 0) } == -1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            unsafe { kill(child_pid, 0) },
            -1,
            "descendant survived stop"
        );

        let _ = fs::remove_file(script);
        let _ = fs::remove_file(marker);
        let _ = fs::remove_file(child_pid_path);
    }
}
