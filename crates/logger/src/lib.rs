use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use chrono::Utc;
use once_cell::sync::OnceCell;

/// Minimal process lifecycle logger. Register per-program paths with `register(program, path)`
/// or a global fallback with `init_global(path)`. Call `log(event)` to write events.
pub enum LogEvent {
	Start { program: String, id: usize, pid: Option<u32> },
	Stop { program: String, id: usize },
	Restart { program: String, id: usize, reason: String },
	Crash { program: String, id: usize, exit_code: Option<i32> },
	ConfigReload { source: Option<String> },
}

static LOGGERS: OnceCell<Mutex<HashMap<String, File>>> = OnceCell::new();
static GLOBAL: OnceCell<Mutex<File>> = OnceCell::new();

/// Initialize a global fallback logger file.
pub fn init_global(path: impl AsRef<Path>) -> std::io::Result<()> {
	let file = OpenOptions::new().create(true).append(true).open(path)?;
	GLOBAL.set(Mutex::new(file)).map_err(|_| {
		std::io::Error::new(std::io::ErrorKind::Other, "global logger already initialized")
	})
}

/// Register a per-program log path. Overwrites existing registration for the same program.
pub fn register(program: &str, path: impl AsRef<Path>) -> std::io::Result<()> {
	let file = OpenOptions::new().create(true).append(true).open(path)?;
	let map = LOGGERS.get_or_init(|| Mutex::new(HashMap::new()));
	let mut m = map.lock().expect("logger map poisoned");
	m.insert(program.to_string(), file);
	Ok(())
}

fn timestamp() -> String {
	Utc::now().to_rfc3339()
}

/// Log an event; selects per-program file if registered, otherwise global if available.
pub fn log(event: LogEvent) -> std::io::Result<()> {
	let (maybe_program, line) = match event {
		LogEvent::Start { program, id, pid } => (Some(program.clone()), format!("[{}] START program={} id={} pid={}\n", timestamp(), program, id, pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()))),
		LogEvent::Stop { program, id } => (Some(program.clone()), format!("[{}] STOP program={} id={}\n", timestamp(), program, id)),
		LogEvent::Restart { program, id, reason } => (Some(program.clone()), format!("[{}] RESTART program={} id={} reason={}\n", timestamp(), program, id, reason)),
		LogEvent::Crash { program, id, exit_code } => (Some(program.clone()), format!("[{}] CRASH program={} id={} exit_code={}\n", timestamp(), program, id, exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".into()))),
		LogEvent::ConfigReload { source } => (None, format!("[{}] CONFIG_RELOAD source={}\n", timestamp(), source.unwrap_or_else(|| "-".into()))),
	};

	if let Some(prog) = maybe_program {
		if let Some(map) = LOGGERS.get() {
			if let Some(mut m) = map.lock().ok() {
				if let Some(file) = m.get_mut(&prog) {
					file.write_all(line.as_bytes())?;
					file.flush()?;
					return Ok(());
				}
			}
		}
	}

	if let Some(global) = GLOBAL.get() {
		let mut g = global.lock().expect("global logger poisoned");
		g.write_all(line.as_bytes())?;
		g.flush()?;
		return Ok(());
	}

	Err(std::io::Error::new(std::io::ErrorKind::Other, "no logger registered for event"))
}