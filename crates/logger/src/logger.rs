use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use chrono::Utc;
use once_cell::sync::OnceCell;
/// Minimal process lifecycle logger. Initialize with `init(path)` then call `log`.
pub enum LogEvent {
	Start { program: String, id: usize, pid: Option<u32> },
	Stop { program: String, id: usize },
	Restart { program: String, id: usize, reason: String },
	Crash { program: String, id: usize, exit_code: Option<i32> },
	ConfigReload { source: Option<String> },
}

static LOGGER: OnceCell<Mutex<std::fs::File>> = OnceCell::new();

/// Initialize the global logger to append to `path`.
pub fn init(path: impl AsRef<Path>) -> std::io::Result<()> {
	let file = OpenOptions::new().create(true).append(true).open(path)?;
	LOGGER.set(Mutex::new(file)).map_err(|_| {
		std::io::Error::new(std::io::ErrorKind::Other, "logger already initialized")
	})
}

fn timestamp() -> String {
	Utc::now().to_rfc3339()
}
/// Log an event; best-effort. Returns Err only if the write fails or logger not initialized.
pub fn log(event: LogEvent) -> std::io::Result<()> {
	let file_mutex = match LOGGER.get() {
		Some(m) => m,
		None => return Err(std::io::Error::new(std::io::ErrorKind::Other, "logger not initialized")),
	};

	let line: String;
	match event {
		LogEvent::Start { program, id, pid } => {
			line = format!("[{}] START program={} id={} pid={}\n", timestamp(), program, id, pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()));
		}
		LogEvent::Stop { program, id } => {
			line = format!("[{}] STOP program={} id={}\n", timestamp(), program, id);
		}
		LogEvent::Restart { program, id, reason } => {
			line = format!("[{}] RESTART program={} id={} reason={}\n", timestamp(), program, id, reason);
		}
		LogEvent::Crash { program, id, exit_code } => {
			line = format!("[{}] CRASH program={} id={} exit_code={}\n", timestamp(), program, id, exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".into()));
		}
		LogEvent::ConfigReload { source } => {
			line = format!("[{}] CONFIG_RELOAD source={}\n", timestamp(), source.unwrap_or_else(|| "-".into()));
		}
	}

	let mut file = file_mutex.lock().expect("logger mutex poisoned");
	file.write_all(line.as_bytes())?;
	file.flush()?;
	Ok(())
}
