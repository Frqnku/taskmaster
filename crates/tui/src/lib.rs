use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use config::model::Config;
use process::Supervisor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Start(String),
    Stop(String),
    Restart(String),
    Status,
    Reload,
    History,
    Help,
    Quit,
}

impl ShellCommand {
    pub fn parse(line: &str) -> Option<Self> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            [] => None,
            [command, name] if command.eq_ignore_ascii_case("start") => {
                Some(Self::Start((*name).to_string()))
            }
            [command, name] if command.eq_ignore_ascii_case("stop") => {
                Some(Self::Stop((*name).to_string()))
            }
            [command, name] if command.eq_ignore_ascii_case("restart") => {
                Some(Self::Restart((*name).to_string()))
            }
            [command] if command.eq_ignore_ascii_case("status") => Some(Self::Status),
            [command] if command.eq_ignore_ascii_case("reload") => Some(Self::Reload),
            [command] if command.eq_ignore_ascii_case("history") => Some(Self::History),
            [command]
                if command.eq_ignore_ascii_case("help") || *command == "h" || *command == "?" =>
            {
                Some(Self::Help)
            }
            [command]
                if command.eq_ignore_ascii_case("quit")
                    || command.eq_ignore_ascii_case("exit")
                    || *command == "q" =>
            {
                Some(Self::Quit)
            }
            _ => None,
        }
    }
}

pub struct Shell;

impl Shell {
    pub fn run(supervisor: &Supervisor, config_path: &Path) -> io::Result<()> {
        let (sender, receiver) = mpsc::channel::<String>();
        thread::spawn(move || {
            let stdin = io::stdin();
            loop {
                let mut buffer = String::new();
                match stdin.read_line(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if sender.send(buffer).is_err() => break,
                    Ok(_) => {}
                }
            }
        });

        Self::print_welcome();
        let mut stdout = io::stdout();
        let mut prompt_needed = true;
        let mut history = Vec::new();

        loop {
            if signals::take_shutdown() {
                println!("\nShutdown signal received.");
                return Ok(());
            }
            if signals::take_reload() {
                println!("\nSIGHUP received. Reloading configuration...");
                Self::handle_reload(supervisor, config_path);
                prompt_needed = true;
            }

            if prompt_needed {
                print!("taskmaster> ");
                stdout.flush()?;
                prompt_needed = false;
            }

            let buffer = match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            let trimmed = buffer.trim();
            if !trimmed.is_empty() {
                history.push(trimmed.to_string());
            }

            match ShellCommand::parse(&buffer) {
                Some(ShellCommand::Start(name)) => {
                    Self::handle_for_targets(supervisor, &name, "start", Supervisor::start_program)
                }
                Some(ShellCommand::Stop(name)) => {
                    Self::handle_for_targets(supervisor, &name, "stop", Supervisor::stop_program)
                }
                Some(ShellCommand::Restart(name)) => Self::handle_restart(supervisor, &name),
                Some(ShellCommand::Status) => Self::handle_status(supervisor),
                Some(ShellCommand::Reload) => Self::handle_reload(supervisor, config_path),
                Some(ShellCommand::History) => {
                    for (index, command) in history.iter().enumerate() {
                        println!("{:>4}  {}", index + 1, command);
                    }
                }
                Some(ShellCommand::Help) => Self::print_help(),
                Some(ShellCommand::Quit) => {
                    println!("Shutting down...");
                    return Ok(());
                }
                None if !trimmed.is_empty() => {
                    println!("Invalid command. Type 'help' for usage.");
                }
                None => {}
            }
            prompt_needed = true;
        }
    }

    fn handle_for_targets(
        supervisor: &Supervisor,
        target: &str,
        verb: &str,
        action: fn(&Supervisor, &str) -> Result<(), process::instance::ProcessError>,
    ) {
        let targets = if target.eq_ignore_ascii_case("all") {
            supervisor.program_names()
        } else {
            vec![target.to_string()]
        };
        for name in targets {
            match action(supervisor, &name) {
                Ok(()) => println!("{}: {}", verb, name),
                Err(error) => eprintln!("{} {} failed: {}", verb, name, error),
            }
        }
    }

    fn handle_restart(supervisor: &Supervisor, target: &str) {
        let targets = if target.eq_ignore_ascii_case("all") {
            supervisor.program_names()
        } else {
            vec![target.to_string()]
        };
        for name in targets {
            if let Err(error) = supervisor.stop_program(&name) {
                eprintln!("restart {} failed while stopping: {}", name, error);
                continue;
            }
            match supervisor.start_program(&name) {
                Ok(()) => println!("restart: {}", name),
                Err(error) => eprintln!("restart {} failed while starting: {}", name, error),
            }
        }
    }

    fn handle_reload(supervisor: &Supervisor, config_path: &Path) {
        let config = match Config::load_from_path(config_path) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("Reload rejected; current configuration kept: {}", error);
                return;
            }
        };
        match supervisor.reload_config(config, Some(config_path.display().to_string())) {
            Ok(summary) => println!(
                "Configuration reloaded. added={:?} removed={:?} changed={:?} unchanged={:?}",
                summary.added, summary.removed, summary.changed, summary.unchanged
            ),
            Err(error) => eprintln!("Configuration applied with a process error: {}", error),
        }
    }

    fn handle_status(supervisor: &Supervisor) {
        let statuses = match supervisor.statuses() {
            Ok(statuses) => statuses,
            Err(error) => {
                eprintln!("status failed: {}", error);
                return;
            }
        };
        if statuses.is_empty() {
            println!("No programs configured.");
            return;
        }

        println!(
            "\n{:<20} {:>8} {:>10} {:>12} {:>10}",
            "PROGRAM", "INSTANCE", "PID", "STATE", "UPTIME"
        );
        println!("{}", "-".repeat(66));
        for status in statuses {
            let pid = status
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string());
            let uptime = status
                .uptime
                .map(|uptime| format!("{}s", uptime.as_secs()))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<20} {:>8} {:>10} {:>12} {:>10}",
                status.program, status.id, pid, status.state, uptime
            );
        }
        println!();
    }

    fn print_welcome() {
        println!("Taskmaster control shell. Type 'help' for commands.");
    }

    fn print_help() {
        println!(
            "\
Commands:
  status                 Show every configured instance and its state
  start <name|all>       Start a program
  stop <name|all>        Gracefully stop a program
  restart <name|all>     Stop, then start a program
  reload                 Reload and selectively apply the config file
  history                Show commands entered in this session
  help                    Show this help
  quit | exit             Stop all children and exit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_commands() {
        assert_eq!(
            ShellCommand::parse("start web"),
            Some(ShellCommand::Start("web".to_string()))
        );
        assert_eq!(
            ShellCommand::parse("stop all"),
            Some(ShellCommand::Stop("all".to_string()))
        );
        assert_eq!(
            ShellCommand::parse("restart api"),
            Some(ShellCommand::Restart("api".to_string()))
        );
        assert_eq!(ShellCommand::parse("status"), Some(ShellCommand::Status));
        assert_eq!(ShellCommand::parse("reload"), Some(ShellCommand::Reload));
        assert_eq!(ShellCommand::parse("history"), Some(ShellCommand::History));
        assert_eq!(ShellCommand::parse("exit"), Some(ShellCommand::Quit));
    }

    #[test]
    fn rejects_missing_or_extra_arguments() {
        assert_eq!(ShellCommand::parse(""), None);
        assert_eq!(ShellCommand::parse("start"), None);
        assert_eq!(ShellCommand::parse("status extra"), None);
        assert_eq!(ShellCommand::parse("unknown"), None);
    }
}
