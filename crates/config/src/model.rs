use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::{error, fmt};

use serde::Deserialize;

/// Validate a signal name (Unix)
#[cfg(unix)]
fn is_valid_signal(name: &str) -> bool {
    if let Ok(n) = name.parse::<i32>() {
        return n > 0 && n < 65;
    }
    matches!(
        name.trim().to_uppercase().as_str(),
        "SIGTERM"
            | "TERM"
            | "SIGKILL"
            | "KILL"
            | "SIGINT"
            | "INT"
            | "SIGHUP"
            | "HUP"
            | "SIGQUIT"
            | "QUIT"
            | "SIGUSR1"
            | "USR1"
            | "SIGUSR2"
            | "USR2"
    )
}

#[cfg(not(unix))]
fn is_valid_signal(_name: &str) -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AutoRestart {
    Always,
    #[default]
    Never,
    Unexpected,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Program {
    pub cmd: String,
    pub numprocs: usize,
    pub autostart: bool,
    pub autorestart: AutoRestart,
    pub exitcodes: Vec<i32>,
    pub startretries: u32,
    /// seconds that a process must stay alive to be considered "started"
    pub starttime: u64,
    /// signal name or number, left for higher-level parsing
    pub stopsignal: Option<String>,
    /// seconds to wait after sending stop signal before force-kill
    pub stoptime: u64,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// optional per-program lifecycle log path
    pub log: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub workingdir: Option<String>,
    pub umask: Option<u32>,
}

impl Default for Program {
    fn default() -> Self {
        Self {
            cmd: String::new(),
            numprocs: 1,
            autostart: false,
            autorestart: AutoRestart::default(),
            exitcodes: vec![0],
            startretries: 0,
            starttime: 1,
            stopsignal: Some("TERM".to_string()),
            stoptime: 5,
            stdout: None,
            stderr: None,
            log: None,
            env: None,
            workingdir: None,
            umask: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub programs: HashMap<String, Program>,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "IO error: {}", e),
            ConfigError::Parse(e) => write!(f, "TOML parse error: {}", e),
            ConfigError::Validation(s) => write!(f, "Validation error: {}", s),
        }
    }
}

impl error::Error for ConfigError {}

impl Config {
    /// Load a `Config` from a TOML file at `path`.
    ///
    /// This performs basic validation (e.g. `numprocs >= 1`, non-empty `cmd`).
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let s = fs::read_to_string(path.as_ref()).map_err(ConfigError::Io)?;
        let cfg: Config = toml::from_str(&s).map_err(ConfigError::Parse)?;
        cfg.validate().map(|_| cfg).map_err(ConfigError::Validation)
    }

    fn validate(&self) -> Result<(), String> {
        if self.programs.is_empty() {
            return Err("no programs defined".into());
        }

        for (name, p) in &self.programs {
            if p.cmd.trim().is_empty() {
                return Err(format!("program '{}' has empty cmd", name));
            }
            if p.numprocs == 0 {
                return Err(format!("program '{}' numprocs must be >= 1", name));
            }
            if p.exitcodes.is_empty() {
                return Err(format!("program '{}' exitcodes must not be empty", name));
            }

            // Validate stopsignal if configured
            if let Some(sig) = &p.stopsignal
                && !is_valid_signal(sig)
            {
                return Err(format!(
                    "program '{}' has invalid stopsignal: {}",
                    name, sig
                ));
            }

            // Validate startretries is reasonable
            if p.startretries > 1000 {
                return Err(format!(
                    "program '{}' startretries too high (max 1000)",
                    name
                ));
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        path.push(format!(
            "taskmaster-{}-{}-{}.toml",
            name,
            std::process::id(),
            unique
        ));
        fs::write(&path, content).expect("write temp config");
        path
    }

    #[test]
    fn program_default_values_are_sane() {
        let program = Program::default();

        assert_eq!(program.numprocs, 1);
        assert!(!program.autostart);
        assert_eq!(program.autorestart, AutoRestart::Never);
        assert_eq!(program.exitcodes, vec![0]);
        assert_eq!(program.startretries, 0);
        assert_eq!(program.starttime, 1);
        assert_eq!(program.stoptime, 5);
        assert_eq!(program.stopsignal.as_deref(), Some("TERM"));
        assert!(program.stdout.is_none());
        assert!(program.stderr.is_none());
    }

    #[test]
    fn parses_minimal_config_with_defaults() {
        let path = temp_file(
            "config-minimal",
            r#"
[programs.web]
cmd = "/bin/true"
"#,
        );

        let config = Config::load_from_path(&path).expect("config should parse");
        let program = config.programs.get("web").expect("missing program");

        assert_eq!(config.programs.len(), 1);
        assert_eq!(program.cmd, "/bin/true");
        assert_eq!(program.numprocs, 1);
        assert_eq!(program.autorestart, AutoRestart::Never);
        assert_eq!(program.exitcodes, vec![0]);
        assert_eq!(program.starttime, 1);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn parses_enum_and_maps_program_fields() {
        let path = temp_file(
            "config-full",
            r#"
[programs.api]
cmd = "/bin/echo hello"
numprocs = 3
autostart = true
autorestart = "unexpected"
exitcodes = [0, 2]
startretries = 4
starttime = 7
stopsignal = "TERM"
stoptime = 9
stdout = "/tmp/api.out"
stderr = "/tmp/api.err"
workingdir = "/tmp"
umask = 18

[programs.api.env]
RUST_LOG = "debug"
PORT = "8080"
"#,
        );

        let config = Config::load_from_path(&path).expect("config should parse");
        let program = config.programs.get("api").expect("missing program");

        assert_eq!(program.numprocs, 3);
        assert!(program.autostart);
        assert_eq!(program.autorestart, AutoRestart::Unexpected);
        assert_eq!(program.exitcodes, vec![0, 2]);
        assert_eq!(program.startretries, 4);
        assert_eq!(program.starttime, 7);
        assert_eq!(program.stopsignal.as_deref(), Some("TERM"));
        assert_eq!(program.stoptime, 9);
        assert_eq!(program.stdout.as_deref(), Some("/tmp/api.out"));
        assert_eq!(program.stderr.as_deref(), Some("/tmp/api.err"));
        assert_eq!(program.workingdir.as_deref(), Some("/tmp"));
        assert_eq!(program.umask, Some(18));
        assert_eq!(
            program.env.as_ref().and_then(|env| env.get("RUST_LOG")),
            Some(&"debug".to_string())
        );
        assert_eq!(
            program.env.as_ref().and_then(|env| env.get("PORT")),
            Some(&"8080".to_string())
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn accepts_zero_start_and_stop_times() {
        let path = temp_file(
            "config-zero-times",
            r#"
[programs.oneshot]
cmd = "exit 0"
starttime = 0
stoptime = 0
"#,
        );

        let config = Config::load_from_path(&path).expect("zero times should be valid");
        let program = config.programs.get("oneshot").expect("missing program");
        assert_eq!(program.starttime, 0);
        assert_eq!(program.stoptime, 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_missing_programs() {
        let path = temp_file("config-empty", "");

        let err = Config::load_from_path(&path).expect_err("empty config should fail");
        assert!(matches!(err, ConfigError::Validation(_)));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_empty_command() {
        let path = temp_file(
            "config-empty-cmd",
            r#"
[programs.web]
cmd = ""
"#,
        );

        let err = Config::load_from_path(&path).expect_err("empty cmd should fail");
        assert!(matches!(err, ConfigError::Validation(msg) if msg.contains("empty cmd")));

        let _ = fs::remove_file(path);
    }
}
