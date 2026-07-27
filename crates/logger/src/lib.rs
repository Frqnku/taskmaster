use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process lifecycle events recorded by Taskmaster.
pub enum LogEvent {
    Start {
        program: String,
        id: usize,
        pid: Option<u32>,
    },
    Stop {
        program: String,
        id: usize,
    },
    Restart {
        program: String,
        id: usize,
        reason: String,
    },
    Crash {
        program: String,
        id: usize,
        exit_code: Option<i32>,
    },
    Fatal {
        program: String,
        id: usize,
        attempts: u32,
    },
    ConfigReload {
        source: Option<String>,
    },
}

static LOGGERS: OnceLock<Mutex<HashMap<String, File>>> = OnceLock::new();
static GLOBAL: OnceLock<Mutex<File>> = OnceLock::new();

/// Initialize the global fallback logger file.
pub fn init_global(path: impl AsRef<Path>) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    GLOBAL
        .set(Mutex::new(file))
        .map_err(|_| std::io::Error::other("global logger already initialized"))
}

/// Register or replace a per-program lifecycle log.
pub fn register(program: &str, path: impl AsRef<Path>) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let map = LOGGERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map
        .lock()
        .map_err(|_| std::io::Error::other("logger map poisoned"))?;
    map.insert(program.to_string(), file);
    Ok(())
}

/// Remove a per-program logger so events use the global fallback.
pub fn unregister(program: &str) -> std::io::Result<()> {
    if let Some(loggers) = LOGGERS.get() {
        let mut loggers = loggers
            .lock()
            .map_err(|_| std::io::Error::other("logger map poisoned"))?;
        loggers.remove(program);
    }
    Ok(())
}

fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}Z", elapsed.as_secs(), elapsed.subsec_millis())
}

/// Write an event to its per-program log, or to the global fallback.
pub fn log(event: LogEvent) -> std::io::Result<()> {
    let (program, line) = match event {
        LogEvent::Start { program, id, pid } => (
            Some(program.clone()),
            format!(
                "[{}] START program={} id={} pid={}\n",
                timestamp(),
                program,
                id,
                pid.map(|pid| pid.to_string())
                    .unwrap_or_else(|| "-".to_string())
            ),
        ),
        LogEvent::Stop { program, id } => (
            Some(program.clone()),
            format!("[{}] STOP program={} id={}\n", timestamp(), program, id),
        ),
        LogEvent::Restart {
            program,
            id,
            reason,
        } => (
            Some(program.clone()),
            format!(
                "[{}] RESTART program={} id={} reason={}\n",
                timestamp(),
                program,
                id,
                reason
            ),
        ),
        LogEvent::Crash {
            program,
            id,
            exit_code,
        } => (
            Some(program.clone()),
            format!(
                "[{}] CRASH program={} id={} exit_code={}\n",
                timestamp(),
                program,
                id,
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            ),
        ),
        LogEvent::Fatal {
            program,
            id,
            attempts,
        } => (
            Some(program.clone()),
            format!(
                "[{}] FATAL program={} id={} attempts={}\n",
                timestamp(),
                program,
                id,
                attempts
            ),
        ),
        LogEvent::ConfigReload { source } => (
            None,
            format!(
                "[{}] CONFIG_RELOAD source={}\n",
                timestamp(),
                source.unwrap_or_else(|| "-".to_string())
            ),
        ),
    };

    if let Some(program) = program
        && let Some(loggers) = LOGGERS.get()
    {
        let mut loggers = loggers
            .lock()
            .map_err(|_| std::io::Error::other("logger map poisoned"))?;
        if let Some(file) = loggers.get_mut(&program) {
            file.write_all(line.as_bytes())?;
            file.flush()?;
            return Ok(());
        }
    }

    if let Some(global) = GLOBAL.get() {
        let mut global = global
            .lock()
            .map_err(|_| std::io::Error::other("global logger poisoned"))?;
        global.write_all(line.as_bytes())?;
        global.flush()?;
        return Ok(());
    }

    Err(std::io::Error::other("no logger registered for event"))
}
