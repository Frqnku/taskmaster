use std::collections::{HashMap, HashSet};
use std::time::Duration;

use config::model::{AutoRestart, Program};
use logger::LogEvent;

use crate::instance::{ProcessError, ProcessInstance, ProcessPoll};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessStatus {
    pub program: String,
    pub id: usize,
    pub pid: Option<u32>,
    pub state: &'static str,
    pub uptime: Option<Duration>,
}

/// All runtime state for one configured program.
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

    /// Spawn every configured instance.
    pub fn start_all(&mut self) -> Result<(), ProcessError> {
        self.stopped_instances.clear();
        self.fatal_instances.clear();
        self.startup_failures.clear();

        let mut first_error = None;
        for id in 0..self.program.numprocs {
            if self.instances.contains_key(&id) {
                continue;
            }
            match ProcessInstance::spawn(&self.name, id, &self.program) {
                Ok(instance) => {
                    self.instances.insert(id, instance);
                }
                Err(error) => {
                    self.fatal_instances.insert(id);
                    let _ = logger::log(LogEvent::Fatal {
                        program: self.name.clone(),
                        id,
                        attempts: 1,
                    });
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Gracefully stop every running instance and prevent auto-restart.
    pub fn stop_all(&mut self) -> Result<(), ProcessError> {
        let mut first_error = None;
        for instance in self.instances.values_mut() {
            if let Err(error) = instance.stop()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        self.instances.clear();
        self.startup_failures.clear();
        self.fatal_instances.clear();
        self.stopped_instances = (0..self.program.numprocs).collect();

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Spawn one instance during retry or autorestart handling.
    pub fn spawn_one(&mut self, id: usize) -> Result<(), ProcessError> {
        let instance = ProcessInstance::spawn(&self.name, id, &self.program)?;
        self.fatal_instances.remove(&id);
        self.stopped_instances.remove(&id);
        self.instances.insert(id, instance);
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        !self.instances.is_empty()
    }

    pub fn running_count(&mut self) -> usize {
        let _ = self.reconcile();
        self.instances.len()
    }

    pub fn statuses(&self) -> Vec<ProcessStatus> {
        (0..self.program.numprocs)
            .map(|id| {
                if let Some(instance) = self.instances.get(&id) {
                    let uptime = instance.started_at.map(|started| started.elapsed());
                    let state = if uptime.unwrap_or_default()
                        >= Duration::from_secs(self.program.starttime)
                    {
                        "RUNNING"
                    } else {
                        "STARTING"
                    };
                    ProcessStatus {
                        program: self.name.clone(),
                        id,
                        pid: instance.pid(),
                        state,
                        uptime,
                    }
                } else {
                    ProcessStatus {
                        program: self.name.clone(),
                        id,
                        pid: None,
                        state: if self.fatal_instances.contains(&id) {
                            "FATAL"
                        } else {
                            "STOPPED"
                        },
                        uptime: None,
                    }
                }
            })
            .collect()
    }

    /// Reap exited children and apply startup retry and autorestart policies.
    pub fn reconcile(&mut self) -> Result<(), ProcessError> {
        let starttime = Duration::from_secs(self.program.starttime);
        let startretries = self.program.startretries;
        let exitcodes = self.program.exitcodes.clone();
        let autorestart = self.program.autorestart;
        let ids: Vec<usize> = self.instances.keys().copied().collect();

        for id in ids {
            let mut reached_running_state = false;
            let outcome = if let Some(instance) = self.instances.get_mut(&id) {
                let started = instance
                    .started_at
                    .map(|started_at| started_at.elapsed() >= starttime)
                    .unwrap_or(false);

                match instance.poll()? {
                    ProcessPoll::Running => {
                        reached_running_state = started;
                        None
                    }
                    ProcessPoll::Exited(exit_code) => Some((!started, exit_code)),
                    ProcessPoll::Stopped => Some((false, None)),
                }
            } else {
                continue;
            };

            if reached_running_state {
                self.startup_failures.insert(id, 0);
            }

            let Some((started_too_early, exit_code)) = outcome else {
                continue;
            };
            self.instances.remove(&id);

            if started_too_early {
                let failures = self.startup_failures.entry(id).or_insert(0);
                *failures += 1;
                let attempts = *failures;
                let _ = logger::log(LogEvent::Crash {
                    program: self.name.clone(),
                    id,
                    exit_code,
                });

                if attempts > startretries {
                    self.fatal_instances.insert(id);
                    let _ = logger::log(LogEvent::Fatal {
                        program: self.name.clone(),
                        id,
                        attempts,
                    });
                    continue;
                }

                let _ = logger::log(LogEvent::Restart {
                    program: self.name.clone(),
                    id,
                    reason: format!("startup retry {}/{}", attempts, startretries),
                });
                if let Err(error) = self.spawn_one(id) {
                    self.fatal_instances.insert(id);
                    let _ = logger::log(LogEvent::Fatal {
                        program: self.name.clone(),
                        id,
                        attempts: attempts + 1,
                    });
                    return Err(error);
                }
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

            if !expected_exit {
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
                if let Err(error) = self.spawn_one(id) {
                    self.fatal_instances.insert(id);
                    let _ = logger::log(LogEvent::Fatal {
                        program: self.name.clone(),
                        id,
                        attempts: 1,
                    });
                    return Err(error);
                }
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
    use std::path::{Path, PathBuf};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
    fn write_script(name: &str, body: &str) -> PathBuf {
        let path = unique_path(name);
        fs::write(&path, format!("#!/bin/sh\n{}\n", body)).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
        path
    }

    #[cfg(windows)]
    fn write_script(name: &str, body: &str) -> PathBuf {
        let mut path = unique_path(name);
        path.set_extension("cmd");
        fs::write(&path, format!("@echo off\r\n{}\r\n", body)).expect("write script");
        path
    }

    fn base_program(script: &Path) -> Program {
        Program {
            cmd: script.display().to_string(),
            ..Program::default()
        }
    }

    #[test]
    fn start_all_spawns_numprocs_instances() {
        #[cfg(unix)]
        let script = write_script("group-start-all", "sleep 2");
        #[cfg(windows)]
        let script = write_script("group-start-all", "ping -n 4 127.0.0.1 >NUL");
        let mut program = base_program(&script);
        program.numprocs = 3;
        let mut group = ProcessGroup::new("web", program);

        group.start_all().expect("start all");
        assert_eq!(group.instances.len(), 3);
        group.stop_all().expect("stop all");
        let _ = fs::remove_file(script);
    }

    #[test]
    fn expected_exit_after_starttime_stays_stopped() {
        #[cfg(unix)]
        let script = write_script("group-expected-exit", "exit 0");
        #[cfg(windows)]
        let script = write_script("group-expected-exit", "exit /b 0");
        let mut program = base_program(&script);
        program.autorestart = AutoRestart::Never;
        let mut group = ProcessGroup::new("web", program);
        group.start_all().expect("start all");
        for instance in group.instances.values_mut() {
            instance.started_at = Some(Instant::now() - Duration::from_secs(2));
        }

        std::thread::sleep(Duration::from_millis(100));
        group.reconcile().expect("reconcile");
        assert!(group.instances.is_empty());
        assert_eq!(group.statuses()[0].state, "STOPPED");
        let _ = fs::remove_file(script);
    }

    #[test]
    fn unexpected_exit_restarts() {
        #[cfg(unix)]
        let script = write_script("group-unexpected-exit", "exit 42");
        #[cfg(windows)]
        let script = write_script("group-unexpected-exit", "exit /b 42");
        let mut program = base_program(&script);
        program.autorestart = AutoRestart::Unexpected;
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
    fn early_death_becomes_fatal_after_retries() {
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

        std::thread::sleep(Duration::from_millis(100));
        group.reconcile().expect("first reconcile");
        std::thread::sleep(Duration::from_millis(100));
        group.reconcile().expect("second reconcile");
        assert!(group.instances.is_empty());
        assert_eq!(group.statuses()[0].state, "FATAL");
        let _ = fs::remove_file(script);
    }

    #[test]
    fn starttime_zero_accepts_short_lived_commands() {
        let program = Program {
            cmd: "exit 0".to_string(),
            starttime: 0,
            startretries: 0,
            ..Program::default()
        };
        let mut group = ProcessGroup::new("oneshot", program);
        group.start_all().expect("start all");
        std::thread::sleep(Duration::from_millis(100));
        group.reconcile().expect("reconcile");
        assert_eq!(group.statuses()[0].state, "STOPPED");
    }
}
