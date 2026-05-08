use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use config::model::Config;
use logger::LogEvent;

use crate::instance::ProcessError;
use crate::manager::ProcessGroup;

pub struct Supervisor {
	groups: Arc<Mutex<HashMap<String, ProcessGroup>>>,
	running: Arc<AtomicBool>,
	handle: Option<JoinHandle<()>>,
	tick: Duration,
}

impl Supervisor {
	pub fn from_config(config: Config) -> Self {
		let mut groups = HashMap::new();
		for (name, program) in &config.programs {
			if let Some(log_path) = &program.log {
				let _ = logger::register(name, log_path);
			}
		}

		for (name, program) in config.programs {
			groups.insert(name.clone(), ProcessGroup::new(name, program));
		}

		Self {
			groups: Arc::new(Mutex::new(groups)),
			running: Arc::new(AtomicBool::new(false)),
			handle: None,
			tick: Duration::from_millis(250),
		}
	}

	/// Replace current groups with new config. Stops existing groups first.
	pub fn reload_config(&self, config: Config) -> Result<(), ProcessError> {
		let mut groups = self.groups.lock().expect("supervisor mutex poisoned");
		for group in groups.values_mut() {
			let _ = group.stop_all();
		}
		groups.clear();
		for (name, program) in &config.programs {
			if let Some(log_path) = &program.log {
				let _ = logger::register(name, log_path);
			}
		}

		for (name, program) in config.programs {
			groups.insert(name.clone(), ProcessGroup::new(name, program));
		}
		let _ = logger::log(LogEvent::ConfigReload { source: None });
		Ok(())
	}

	pub fn start(&mut self) {
		if self.handle.is_some() {
			return;
		}

		self.running.store(true, Ordering::SeqCst);
		let groups = Arc::clone(&self.groups);
		let running = Arc::clone(&self.running);
		let tick = self.tick;

		self.handle = Some(thread::spawn(move || {
			while running.load(Ordering::SeqCst) {
				if let Ok(mut groups) = groups.lock() {
					for group in groups.values_mut() {
						let _ = group.reconcile();
					}
				}
				thread::sleep(tick);
			}
		}));
	}

	pub fn stop(&mut self) {
		self.running.store(false, Ordering::SeqCst);
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
	}

	pub fn start_program(&self, name: &str) -> Result<(), ProcessError> {
		let mut groups = self.groups.lock()
			.map_err(|_| ProcessError::Spawn("supervisor lock poisoned".into()))?;
		let group = groups
			.get_mut(name)
			.ok_or_else(|| ProcessError::Spawn(format!("unknown program '{}'", name)))?;
		group.start_all()
	}

	pub fn stop_program(&self, name: &str) -> Result<(), ProcessError> {
		let mut groups = self.groups.lock()
			.map_err(|_| ProcessError::Spawn("supervisor lock poisoned".into()))?;
		let group = groups
			.get_mut(name)
			.ok_or_else(|| ProcessError::Spawn(format!("unknown program '{}'", name)))?;
		group.stop_all()
	}

	pub fn reconcile_once(&self) -> Result<(), ProcessError> {
		let mut groups = self.groups.lock()
			.map_err(|_| ProcessError::Spawn("supervisor lock poisoned".into()))?;
		for group in groups.values_mut() {
			group.reconcile()?;
		}
		Ok(())
	}

	pub fn instance_count(&self, name: &str) -> Option<usize> {
		let groups = self.groups.lock().ok()?;
		groups.get(name).map(|group| group.instances.len())
	}

	/// Get list of all program names
	pub fn program_names(&self) -> Vec<String> {
		self.groups.lock()
			.ok()
			.map(|groups| groups.keys().cloned().collect())
			.unwrap_or_default()
	}

	/// Get whether supervisor is still running
	pub fn is_running(&self) -> bool {
		self.running.load(Ordering::SeqCst)
	}

	/// Wait for all instances to be stopped (for graceful shutdown)
	/// Returns true if all stopped, false if timeout
	pub fn wait_for_all_stopped(&self, timeout_secs: u64) -> bool {
		use std::time::{Duration, Instant};
		let start = Instant::now();
		let timeout = Duration::from_secs(timeout_secs);
		
		loop {
			if let Ok(groups) = self.groups.lock() {
				let total_running: usize = groups.values()
					.map(|group| group.instances.len())
					.sum();
				if total_running == 0 {
					return true;
				}
			}
			
			if start.elapsed() > timeout {
				return false;
			}
			
			std::thread::sleep(Duration::from_millis(100));
		}
	}
}

impl Drop for Supervisor {
	fn drop(&mut self) {
		self.stop();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use config::model::Program;

	use std::fs;
	use std::path::PathBuf;
	use std::time::{SystemTime, UNIX_EPOCH};

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
	fn write_script(name: &str, body: &str) -> PathBuf {
		let path = unique_path(name);
		let script = format!("#!/bin/sh\n{}\n", body);
		fs::write(&path, script).expect("write script");

		let perms = fs::Permissions::from_mode(0o755);
		fs::set_permissions(&path, perms).expect("chmod script");

		path
	}

	#[cfg(windows)]
	fn write_script(name: &str, body: &str) -> PathBuf {
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

	#[test]
	fn supervisor_starts_and_stops_programs() {
		#[cfg(unix)]
		let script = write_script("supervisor-start-stop", "sleep 1");
		#[cfg(windows)]
		let script = write_script("supervisor-start-stop", "timeout /T 1 /NOBREAK >NUL");
		let mut config = Config::default();
		config.programs.insert(
			"web".to_string(),
			Program {
				cmd: command_for(&script),
				starttime: 1,
				..Program::default()
			},
		);

		let supervisor = Supervisor::from_config(config);
		assert_eq!(supervisor.instance_count("web"), Some(0));

		supervisor.start_program("web").expect("start program");
		assert_eq!(supervisor.instance_count("web"), Some(1));

		supervisor.stop_program("web").expect("stop program");
		assert_eq!(supervisor.instance_count("web"), Some(0));

		let _ = fs::remove_file(script);
	}

	#[test]
	fn supervisor_reports_unknown_program() {
		let supervisor = Supervisor::from_config(Config::default());
		let err = supervisor
			.start_program("missing")
			.expect_err("unknown program should fail");

		match err {
			ProcessError::Spawn(msg) => assert!(msg.contains("unknown program")),
			other => panic!("unexpected error: {:?}", other),
		}
	}
}
