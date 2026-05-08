use std::collections::{HashMap, HashSet};

use config::model::{AutoRestart, Program};
use logger::LogEvent;

use crate::instance::{ProcessError, ProcessInstance, ProcessPoll};

/// Manages a group of process instances for a single program (numprocs).
pub struct ProcessGroup {
	pub name: String,
	pub program: Program,
	pub instances: HashMap<usize, ProcessInstance>,
	startup_failures: HashMap<usize, u32>,
	fatal_instances: HashSet<usize>,
	stopped_instances: HashSet<usize>,
}

impl ProcessGroup {
	pub fn new(name: impl Into<String>, program: Program) -> Self {
		Self {
			name: name.into(),
			program,
			instances: HashMap::new(),
			startup_failures: HashMap::new(),
			fatal_instances: HashSet::new(),
			stopped_instances: HashSet::new(),
		}
	}

	/// Spawn `numprocs` instances according to the program configuration.
	pub fn start_all(&mut self) -> Result<(), ProcessError> {
		self.stopped_instances.clear();
		self.fatal_instances.clear();
		self.startup_failures.clear();

		let n = self.program.numprocs;
		for i in 0..n {
			if self.instances.contains_key(&i) {
				continue;
			}
			let inst = ProcessInstance::spawn(&self.name, i, &self.program)?;
			self.instances.insert(i, inst);
		}
		Ok(())
	}

	/// Stop all running instances.
	pub fn stop_all(&mut self) -> Result<(), ProcessError> {
		for (id, inst) in self.instances.iter_mut() {
			let _ = inst.stop();
			self.stopped_instances.insert(*id);
		}
		self.instances.clear();
		self.startup_failures.clear();
		Ok(())
	}

	/// Spawn a single instance (used for backoff/restart logic).
	pub fn spawn_one(&mut self, id: usize) -> Result<(), ProcessError> {
		self.fatal_instances.remove(&id);
		self.stopped_instances.remove(&id);
		let inst = ProcessInstance::spawn(&self.name, id, &self.program)?;
		self.instances.insert(id, inst);
		Ok(())
	}

	/// Check running status of all instances and return how many are running.
	pub fn running_count(&mut self) -> usize {
		let mut running = 0;
		let mut to_remove = Vec::new();
		let ids: Vec<usize> = self.instances.keys().copied().collect();
		for id in ids {
			if let Some(inst) = self.instances.get_mut(&id) {
				match inst.poll() {
					Ok(ProcessPoll::Running) => {
						running += 1;
					}
					Ok(ProcessPoll::Exited(_)) | Ok(ProcessPoll::Stopped) | Err(_) => {
						let _ = inst.stop();
						to_remove.push(id);
					}
				}
			}
		}
		for id in to_remove {
			self.instances.remove(&id);
		}
		running
	}

	/// One reconciliation tick: reap dead children and apply restart policy.
	pub fn reconcile(&mut self) -> Result<(), ProcessError> {
		let starttime = self.program.starttime;
		let startretries = self.program.startretries;
		let exitcodes = self.program.exitcodes.clone();
		let autorestart = self.program.autorestart;
		let ids: Vec<usize> = self.instances.keys().copied().collect();
		for id in ids {
			let mut reset_startup_failures = false;
			let outcome = if let Some(instance) = self.instances.get_mut(&id) {
				let started = instance
					.started_at
					.map(|started_at| started_at.elapsed().as_secs() >= starttime)
					.unwrap_or(false);

				match instance.poll()? {
					ProcessPoll::Running => {
						if started {
							reset_startup_failures = true;
						}
						None
					}
					ProcessPoll::Exited(exit_code) => Some((!started, exit_code)),
					ProcessPoll::Stopped => Some((false, None)),
				}
			} else {
				continue;
			};

			if reset_startup_failures {
				self.startup_failures.insert(id, 0);
			}

			let Some((started_too_early, exit_code)) = outcome else {
				continue;
			};

			self.instances.remove(&id);
			if started_too_early {
				let failures = self.startup_failures.entry(id).or_insert(0);
				*failures += 1;
				if *failures > startretries {
					self.fatal_instances.insert(id);
					continue;
				}
				let _ = logger::log(LogEvent::Restart {
					program: self.name.clone(),
					id,
					reason: "start_early".to_string(),
				});
				self.spawn_one(id)?;
				continue;
			}

			let expected_exit = exit_code
				.map(|code| exitcodes.contains(&code))
				.unwrap_or(false);
			let should_restart = match autorestart {
				AutoRestart::Always => true,
				AutoRestart::Never => false,
				AutoRestart::Unexpected => !expected_exit,
			};

			if !expected_exit && exit_code.is_some() {
				let _ = logger::log(LogEvent::Crash {
					program: self.name.clone(),
					id,
					exit_code,
				});
			}

			if should_restart {
				let _ = logger::log(LogEvent::Restart {
					program: self.name.clone(),
					id,
					reason: format!("autorestart={:?}", autorestart),
				});
				self.spawn_one(id)?;
			} else {
				self.startup_failures.remove(&id);
				self.stopped_instances.insert(id);
				let _ = logger::log(LogEvent::Stop {
					program: self.name.clone(),
					id,
				});
			}
		}

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use std::fs;
	use std::path::PathBuf;
	use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

	fn base_program(script: &PathBuf) -> Program {
		Program {
			cmd: command_for(script),
			starttime: 1,
			..Program::default()
		}
	}

	#[test]
	fn start_all_spawns_numprocs_instances() {
		#[cfg(unix)]
		let script = write_script("group-start-all", "sleep 1");
		#[cfg(windows)]
		let script = write_script("group-start-all", "timeout /T 1 /NOBREAK >NUL");
		let mut program = base_program(&script);
		program.numprocs = 3;

		let mut group = ProcessGroup::new("web", program);
		group.start_all().expect("start all");

		assert_eq!(group.instances.len(), 3);
		assert!(group.instances.contains_key(&0));
		assert!(group.instances.contains_key(&1));
		assert!(group.instances.contains_key(&2));

		group.stop_all().expect("stop all");
		let _ = fs::remove_file(script);
	}

	#[test]
	fn reconcile_keeps_expected_exit_stopped() {
		#[cfg(unix)]
		let script = write_script("group-expected-exit", "exit 0");
		#[cfg(windows)]
		let script = write_script("group-expected-exit", "exit /b 0");
		let mut program = base_program(&script);
		program.autorestart = AutoRestart::Never;
		program.exitcodes = vec![0];
		program.startretries = 0;

		let mut group = ProcessGroup::new("web", program);
		group.start_all().expect("start all");

		for instance in group.instances.values_mut() {
			instance.started_at = Some(Instant::now() - Duration::from_secs(2));
		}

		std::thread::sleep(Duration::from_millis(100));
		group.reconcile().expect("reconcile");

		assert_eq!(group.instances.len(), 0);

		let _ = fs::remove_file(script);
	}

	#[test]
	fn reconcile_restarts_unexpected_exit() {
		#[cfg(unix)]
		let script = write_script("group-unexpected-exit", "exit 42");
		#[cfg(windows)]
		let script = write_script("group-unexpected-exit", "exit /b 42");
		let mut program = base_program(&script);
		program.autorestart = AutoRestart::Unexpected;
		program.exitcodes = vec![0];
		program.startretries = 0;

		let mut group = ProcessGroup::new("web", program);
		group.start_all().expect("start all");

		for instance in group.instances.values_mut() {
			instance.started_at = Some(Instant::now() - Duration::from_secs(2));
		}

		std::thread::sleep(Duration::from_millis(100));
		group.reconcile().expect("reconcile");

		assert_eq!(group.instances.len(), 1);

		group.stop_all().expect("stop all");
		let _ = fs::remove_file(script);
	}

	#[test]
	fn reconcile_respects_startretries_for_early_death() {
		#[cfg(unix)]
		let script = write_script("group-startretries", "exit 1");
		#[cfg(windows)]
		let script = write_script("group-startretries", "exit /b 1");
		let mut program = base_program(&script);
		program.autorestart = AutoRestart::Always;
		program.starttime = 10;
		program.startretries = 1;

		let mut group = ProcessGroup::new("web", program);
		group.start_all().expect("start all");

		std::thread::sleep(Duration::from_millis(200));
		group.reconcile().expect("first reconcile");
		assert_eq!(group.instances.len(), 1);

		std::thread::sleep(Duration::from_secs(1));
		group.reconcile().expect("second reconcile");
		assert_eq!(group.instances.len(), 0);

		let _ = fs::remove_file(script);
	}
}
