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

#[derive(Debug)]
pub enum ProcessError {
	Io(io::Error),
	Spawn(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPoll {
	Running,
	Exited(Option<i32>),
	Stopped,
}

impl From<io::Error> for ProcessError {
	fn from(e: io::Error) -> Self {
		ProcessError::Io(e)
	}
}

/// Represents a single supervised child process instance.
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
	/// Spawn a new process according to `program` configuration.
	/// `id` is an instance index (0..numprocs).
	pub fn spawn(name: &str, id: usize, program: &Program) -> Result<Self, ProcessError> {
		// Prepare stdout/stderr targets.
		let stdout_path = program.stdout.as_ref().map(|p| PathBuf::from(p));
		let stderr_path = program.stderr.as_ref().map(|p| PathBuf::from(p));

		let stdout_stdio = match &stdout_path {
			Some(p) => {
				let f = OpenOptions::new().create(true).append(true).open(p)?;
				Stdio::from(f)
			}
			None => {
				let f = OpenOptions::new().write(true).open(null_device_path())?;
				Stdio::from(f)
			}
		};

		let stderr_stdio = match &stderr_path {
			Some(p) => {
				let f = OpenOptions::new().create(true).append(true).open(p)?;
				Stdio::from(f)
			}
			None => {
				let f = OpenOptions::new().write(true).open(null_device_path())?;
				Stdio::from(f)
			}
		};

		// Parse the command with a shell-aware parser (handles quotes/escapes).
		let parts = shlex::split(&program.cmd).ok_or(ProcessError::Spawn(
			"failed to parse cmd with shell lexer".into(),
		))?;

		if parts.is_empty() {
			return Err(ProcessError::Spawn("empty cmd".into()));
		}

		let prog = parts[0].clone();
		let args = &parts[1..];
		let mut cmd = Command::new(prog);
		if !args.is_empty() {
			cmd.args(args);
		}

		if let Some(dir) = &program.workingdir {
			cmd.current_dir(dir);
		}

		if let Some(envs) = &program.env {
			for (k, v) in envs {
				cmd.env(k, v);
			}
		}

		cmd.stdout(stdout_stdio).stderr(stderr_stdio);

		// Apply process group isolation and umask on Unix via pre_exec.
		#[cfg(unix)]
		{
			use std::os::unix::process::CommandExt;
			let umask_val = program.umask.map(|m| m as libc::mode_t);
			cmd.pre_exec(move || {
				// Create a new process group to isolate children
				// This prevents signals to supervisor from affecting children
				unsafe {
					libc::setpgid(0, 0);
				}
				// Apply umask if configured
				if let Some(mask) = umask_val {
					unsafe {
						libc::umask(mask);
					}
				}
				Ok(())
			});
		}

		// Spawn the process
		let child = cmd.spawn().map_err(ProcessError::Io)?;
		let pid = child.id();
		let _ = logger::log(LogEvent::Start {
			program: name.to_string(),
			id,
			pid: Some(pid),
		});

		Ok(ProcessInstance {
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

	/// Attempt to stop the child (gentle kill) and wait for it to exit.
	pub fn stop(&mut self) -> Result<(), ProcessError> {
		if let Some(mut c) = self.child.take() {
			let pid = c.id();

			// Attempt graceful shutdown using configured stopsignal and stoptime on Unix.
			#[cfg(unix)]
			{
				if let Some(sig_str) = &self.stopsignal {
					if let Ok(signum) = map_signal_name(sig_str) {
						unsafe { libc::kill(pid as libc::pid_t, signum); }
						// wait up to stoptime seconds for exit
						let start = Instant::now();
						while start.elapsed().as_secs() < self.stoptime {
							match c.try_wait() {
								Ok(Some(_)) => break,
								Ok(None) => thread::sleep(Duration::from_millis(100)),
								Err(_) => break,
							}
						}
						// if still running, fall through to force kill
					}
				}
			}

			// On non-unix or if no stopsignal/stoptime applied, or still running, force kill.
			let _ = c.kill();
			let _ = c.wait();
			let _ = logger::log(LogEvent::Stop {
				program: self.name.clone(),
				id: self.id,
			});
			let _ = pid; // pid captured for potential debugging
		}
		Ok(())
	}

	/// Check whether the child is still running (non-blocking).
	pub fn is_running(&mut self) -> bool {
		if let Some(ref mut c) = self.child {
			match c.try_wait() {
				Ok(Some(_status)) => {
					// exited
					false
				}
				Ok(None) => true,
				Err(_) => false,
			}
		} else {
			false
		}
	}

	/// Poll the child process and return its current state.
	pub fn poll(&mut self) -> Result<ProcessPoll, ProcessError> {
		match self.child.as_mut() {
			Some(child) => match child.try_wait().map_err(ProcessError::Io)? {
				Some(status) => Ok(ProcessPoll::Exited(exit_code(status))),
				None => Ok(ProcessPoll::Running),
			},
			None => Ok(ProcessPoll::Stopped),
		}
	}
}

fn exit_code(status: ExitStatus) -> Option<i32> {
	status.code()
}

#[cfg(unix)]
fn map_signal_name(name: &str) -> Result<libc::c_int, ()> {
	if let Ok(n) = name.parse::<i32>() {
		return Ok(n as libc::c_int);
	}
	match name.trim().to_uppercase().as_str() {
		"SIGTERM" | "TERM" => Ok(libc::SIGTERM),
		"SIGKILL" | "KILL" => Ok(libc::SIGKILL),
		"SIGINT" | "INT" => Ok(libc::SIGINT),
		"SIGHUP" | "HUP" => Ok(libc::SIGHUP),
		"SIGQUIT" | "QUIT" => Ok(libc::SIGQUIT),
		"SIGUSR1" | "USR1" => Ok(libc::SIGUSR1),
		"SIGUSR2" | "USR2" => Ok(libc::SIGUSR2),
		_ => Err(()),
	}
}

#[cfg(not(unix))]
#[allow(dead_code)]
fn map_signal_name(_name: &str) -> Result<i32, ()> {
	// Signals are not available on Windows in the same way; return Err to fall back to force kill.
	Err(())
}

fn null_device_path() -> &'static str {
	#[cfg(unix)]
	{
		"/dev/null"
	}

	#[cfg(windows)]
	{
		"NUL"
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use std::fs;
	use std::path::PathBuf;
	use std::time::{Duration, SystemTime, UNIX_EPOCH};

	#[cfg(unix)]
	use std::os::unix::fs::PermissionsExt;

	fn unique_path(name: &str) -> PathBuf {
		let mut path = std::env::temp_dir();
		let unique = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("system time before unix epoch")
			.as_nanos();
		path.push(format!("taskmaster-{}-{}-{}", name, std::process::id(), unique));
		path
	}

    #[cfg(unix)]
    fn write_executable_script(name: &str, body: &str) -> PathBuf {
        let path = unique_path(name);
        let script = format!("#!/bin/sh\n{}\n", body);
        fs::write(&path, script).expect("write script");

        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod script");

        path
    }

    #[cfg(windows)]
    fn write_executable_script(name: &str, body: &str) -> PathBuf {
        let path = unique_path(name);
        let script = format!("@echo off\r\n{}\r\n", body);
        fs::write(&path, script).expect("write script");
        path
    }

    #[cfg(unix)]
    fn command_for(path: &PathBuf) -> String {
        path.display().to_string()
    }

    #[cfg(windows)]
    fn command_for(path: &PathBuf) -> String {
        format!("cmd /C {}", path.display())
    }

    fn program_for(script: &PathBuf) -> Program {
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
        let script = write_executable_script("instance-running", "timeout /T 1 /NOBREAK >NUL");
        let program = program_for(&script);

        let mut instance = ProcessInstance::spawn("web", 0, &program).expect("spawn child");
        assert_eq!(instance.id, 0);
        assert_eq!(instance.name, "web");
        assert!(instance.child.is_some());
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
			r#"echo hello-out
echo hello-err 1>&2
exit 0"#,
		);
		#[cfg(unix)]
		let mut program = program_for(&script);

		#[cfg(windows)]
		let mut program = Program {
			cmd: "cmd /C echo hello-out & echo hello-err 1>&2".to_string(),
			..Program::default()
		};

        let stdout_path = unique_path("stdout.log");
        let stderr_path = unique_path("stderr.log");

        program.stdout = Some(stdout_path.display().to_string());
        program.stderr = Some(stderr_path.display().to_string());

        let mut instance = ProcessInstance::spawn("web", 1, &program).expect("spawn child");
        std::thread::sleep(Duration::from_millis(150));
        let _ = instance.poll().expect("poll child");
        instance.stop().expect("stop child");

        let stdout = fs::read(&stdout_path).expect("read stdout file");
        let stderr = fs::read(&stderr_path).expect("read stderr file");

        assert!(!stdout.is_empty());
        assert!(!stderr.is_empty());

		#[cfg(unix)]
		let _ = fs::remove_file(script);
        let _ = fs::remove_file(stdout_path);
        let _ = fs::remove_file(stderr_path);
    }

    #[test]
    fn returns_spawn_error_for_empty_command() {
        let program = Program::default();
        let result = ProcessInstance::spawn("web", 0, &program);

        match result {
            Err(ProcessError::Spawn(msg)) => assert!(msg.contains("empty cmd")),
            Err(other) => panic!("unexpected error: {:?}", other),
            Ok(_) => panic!("empty cmd should not spawn"),
        }
    }
}
