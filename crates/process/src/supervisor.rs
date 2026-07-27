use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use config::model::Config;
use logger::LogEvent;

use crate::instance::ProcessError;
use crate::manager::{ProcessGroup, ProcessStatus};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadSummary {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub unchanged: Vec<String>,
}

impl ReloadSummary {
    fn sort(&mut self) {
        self.added.sort();
        self.removed.sort();
        self.changed.sort();
        self.unchanged.sort();
    }
}

pub struct Supervisor {
    groups: Arc<Mutex<HashMap<String, ProcessGroup>>>,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    tick: Duration,
}

impl Supervisor {
    pub fn from_config(config: Config) -> Self {
        let mut groups = HashMap::new();
        for (name, program) in config.programs {
            if let Some(log_path) = &program.log {
                let _ = logger::register(&name, log_path);
            }
            groups.insert(name.clone(), ProcessGroup::new(name, program));
        }

        Self {
            groups: Arc::new(Mutex::new(groups)),
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
            tick: Duration::from_millis(100),
        }
    }

    /// Apply a validated config diff. Unchanged groups and PIDs are preserved.
    pub fn reload_config(
        &self,
        config: Config,
        source: Option<String>,
    ) -> Result<ReloadSummary, ProcessError> {
        for (name, program) in &config.programs {
            if let Some(log_path) = &program.log {
                logger::register(name, log_path)?;
            }
        }

        let mut groups = self
            .groups
            .lock()
            .map_err(|_| ProcessError::Spawn("supervisor lock poisoned".to_string()))?;
        let mut summary = ReloadSummary::default();
        let mut first_error = None;

        let removed: Vec<String> = groups
            .keys()
            .filter(|name| !config.programs.contains_key(*name))
            .cloned()
            .collect();
        for name in removed {
            if let Some(mut group) = groups.remove(&name)
                && let Err(error) = group.stop_all()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            let _ = logger::unregister(&name);
            summary.removed.push(name);
        }

        for (name, program) in config.programs {
            if let Some(group) = groups.get_mut(&name) {
                if group.program == program {
                    summary.unchanged.push(name);
                    continue;
                }

                let was_active = group.is_active();
                if let Err(error) = group.stop_all()
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
                if program.log.is_none() {
                    let _ = logger::unregister(&name);
                }

                let should_start = was_active || program.autostart;
                let mut replacement = ProcessGroup::new(name.clone(), program);
                if should_start
                    && let Err(error) = replacement.start_all()
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
                *group = replacement;
                summary.changed.push(name);
            } else {
                if program.log.is_none() {
                    let _ = logger::unregister(&name);
                }
                let autostart = program.autostart;
                let mut group = ProcessGroup::new(name.clone(), program);
                if autostart
                    && let Err(error) = group.start_all()
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
                groups.insert(name.clone(), group);
                summary.added.push(name);
            }
        }

        summary.sort();
        let _ = logger::log(LogEvent::ConfigReload { source });
        match first_error {
            Some(error) => Err(error),
            None => Ok(summary),
        }
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

    /// Stop supervision, then gracefully terminate every attached process group.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if let Ok(mut groups) = self.groups.lock() {
            for group in groups.values_mut() {
                let _ = group.stop_all();
            }
        }
    }

    pub fn start_program(&self, name: &str) -> Result<(), ProcessError> {
        let mut groups = self
            .groups
            .lock()
            .map_err(|_| ProcessError::Spawn("supervisor lock poisoned".to_string()))?;
        groups
            .get_mut(name)
            .ok_or_else(|| ProcessError::Spawn(format!("unknown program '{}'", name)))?
            .start_all()
    }

    pub fn stop_program(&self, name: &str) -> Result<(), ProcessError> {
        let mut groups = self
            .groups
            .lock()
            .map_err(|_| ProcessError::Spawn("supervisor lock poisoned".to_string()))?;
        groups
            .get_mut(name)
            .ok_or_else(|| ProcessError::Spawn(format!("unknown program '{}'", name)))?
            .stop_all()
    }

    pub fn reconcile_once(&self) -> Result<(), ProcessError> {
        let mut groups = self
            .groups
            .lock()
            .map_err(|_| ProcessError::Spawn("supervisor lock poisoned".to_string()))?;
        for group in groups.values_mut() {
            group.reconcile()?;
        }
        Ok(())
    }

    pub fn statuses(&self) -> Result<Vec<ProcessStatus>, ProcessError> {
        let mut groups = self
            .groups
            .lock()
            .map_err(|_| ProcessError::Spawn("supervisor lock poisoned".to_string()))?;
        let mut statuses = Vec::new();
        for group in groups.values_mut() {
            group.reconcile()?;
            statuses.extend(group.statuses());
        }
        statuses.sort_by(|left, right| {
            left.program
                .cmp(&right.program)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(statuses)
    }

    pub fn instance_count(&self, name: &str) -> Option<usize> {
        let groups = self.groups.lock().ok()?;
        groups.get(name).map(|group| group.instances.len())
    }

    pub fn program_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .groups
            .lock()
            .ok()
            .map(|groups| groups.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn wait_for_all_stopped(&self, timeout_secs: u64) -> bool {
        let started = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        loop {
            if self
                .groups
                .lock()
                .map(|groups| groups.values().all(|group| group.instances.is_empty()))
                .unwrap_or(false)
            {
                return true;
            }
            if started.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
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
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

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

    fn long_running_program(script: &Path) -> Program {
        Program {
            cmd: script.display().to_string(),
            starttime: 0,
            ..Program::default()
        }
    }

    #[test]
    fn supervisor_starts_and_stops_programs() {
        #[cfg(unix)]
        let script = write_script("supervisor-start-stop", "sleep 3");
        #[cfg(windows)]
        let script = write_script("supervisor-start-stop", "ping -n 5 127.0.0.1 >NUL");
        let mut config = Config::default();
        config
            .programs
            .insert("web".to_string(), long_running_program(&script));
        let supervisor = Supervisor::from_config(config);

        supervisor.start_program("web").expect("start program");
        assert_eq!(supervisor.instance_count("web"), Some(1));
        supervisor.stop_program("web").expect("stop program");
        assert_eq!(supervisor.instance_count("web"), Some(0));
        let _ = fs::remove_file(script);
    }

    #[test]
    fn reload_applies_a_selective_config_diff() {
        #[cfg(unix)]
        let script = write_script("supervisor-reload", "sleep 5");
        #[cfg(windows)]
        let script = write_script("supervisor-reload", "ping -n 7 127.0.0.1 >NUL");
        let program = long_running_program(&script);
        let mut initial = Config::default();
        initial.programs.insert("keep".to_string(), program.clone());
        initial
            .programs
            .insert("change".to_string(), program.clone());
        initial
            .programs
            .insert("remove".to_string(), program.clone());
        let mut supervisor = Supervisor::from_config(initial);
        for name in ["keep", "change", "remove"] {
            supervisor.start_program(name).expect("start program");
        }
        let original_pid = supervisor
            .statuses()
            .expect("statuses")
            .into_iter()
            .find(|status| status.program == "keep")
            .expect("keep status")
            .pid;

        let mut changed_program = program.clone();
        changed_program.numprocs = 2;
        let mut reloaded = Config::default();
        reloaded
            .programs
            .insert("keep".to_string(), program.clone());
        reloaded
            .programs
            .insert("change".to_string(), changed_program);
        reloaded.programs.insert(
            "added".to_string(),
            Program {
                cmd: script.display().to_string(),
                autostart: true,
                starttime: 0,
                ..Program::default()
            },
        );
        let summary = supervisor
            .reload_config(reloaded, Some("test".to_string()))
            .expect("reload");

        assert_eq!(summary.added, vec!["added"]);
        assert_eq!(summary.removed, vec!["remove"]);
        assert_eq!(summary.changed, vec!["change"]);
        assert_eq!(summary.unchanged, vec!["keep"]);
        let keep = supervisor
            .statuses()
            .expect("statuses")
            .into_iter()
            .find(|status| status.program == "keep")
            .expect("keep status");
        assert_eq!(keep.pid, original_pid);
        assert_eq!(supervisor.instance_count("added"), Some(1));
        assert_eq!(supervisor.instance_count("change"), Some(2));
        assert_eq!(supervisor.instance_count("remove"), None);

        supervisor.stop();
        let _ = fs::remove_file(script);
    }

    #[test]
    fn shutdown_stops_attached_processes() {
        #[cfg(unix)]
        let script = write_script("supervisor-shutdown", "sleep 5");
        #[cfg(windows)]
        let script = write_script("supervisor-shutdown", "ping -n 7 127.0.0.1 >NUL");
        let mut config = Config::default();
        config
            .programs
            .insert("web".to_string(), long_running_program(&script));
        let mut supervisor = Supervisor::from_config(config);
        supervisor.start_program("web").expect("start program");
        supervisor.start();

        supervisor.stop();
        assert!(supervisor.wait_for_all_stopped(1));
        let _ = fs::remove_file(script);
    }
}
