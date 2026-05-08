/// Interactive shell for process supervision.
///
/// Provides a REPL interface for controlling the supervisor.
/// Commands: start, stop, restart, status, reload, quit/exit, help

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use process::Supervisor;

#[derive(Debug, Clone)]
pub enum ShellCommand {
    Start(String),
    Stop(String),
    Restart(String),
    Status,
    Reload,
    Help,
    Quit,
}

impl ShellCommand {
    /// Parse a line of input into a command
    pub fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        match parts.get(0).map(|s| s.to_lowercase()).as_deref() {
            Some("start") => {
                parts.get(1).map(|name| ShellCommand::Start(name.to_string()))
            }
            Some("stop") => {
                parts.get(1).map(|name| ShellCommand::Stop(name.to_string()))
            }
            Some("restart") => {
                parts.get(1).map(|name| ShellCommand::Restart(name.to_string()))
            }
            Some("status") => Some(ShellCommand::Status),
            Some("reload") => Some(ShellCommand::Reload),
            Some("help" | "h" | "?") => Some(ShellCommand::Help),
            Some("quit" | "exit" | "q") => Some(ShellCommand::Quit),
            _ => None,
        }
    }
}

/// Interactive shell for supervisor control
pub struct Shell;

impl Shell {
    /// Run the interactive shell
    pub fn run(supervisor: &Supervisor, running: Arc<AtomicBool>) -> io::Result<()> {
        let mut stdout = io::stdout();
        let (tx, rx) = mpsc::channel::<String>();

        thread::spawn(move || {
            let stdin = io::stdin();
            loop {
                let mut buffer = String::new();
                match stdin.read_line(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(buffer).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self::print_welcome();

        let mut prompt_needed = true;

        loop {
            if !running.load(Ordering::SeqCst) {
                println!();
                return Ok(());
            }

            if prompt_needed {
                print!("taskmaster> ");
                stdout.flush()?;
                prompt_needed = false;
            }

            let buffer = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };

            // Parse and execute
            if let Some(cmd) = ShellCommand::parse(&buffer) {
                match cmd {
                    ShellCommand::Start(name) => {
                        Self::handle_start(supervisor, &name);
                    }
                    ShellCommand::Stop(name) => {
                        Self::handle_stop(supervisor, &name);
                    }
                    ShellCommand::Restart(name) => {
                        Self::handle_restart(supervisor, &name);
                    }
                    ShellCommand::Status => {
                        Self::handle_status(supervisor);
                    }
                    ShellCommand::Reload => {
                        println!("Reload not yet implemented.");
                    }
                    ShellCommand::Help => {
                        Self::print_help();
                    }
                    ShellCommand::Quit => {
                        println!("Exiting...");
                        running.store(false, Ordering::SeqCst);
                        return Ok(());
                    }
                }
            } else if !buffer.trim().is_empty() {
                println!("Unknown command. Type 'help' for available commands.");
            }

            prompt_needed = true;
        }
    }

    fn print_welcome() {
        println!("\n╔═══════════════════════════════════════╗");
        println!("║        Taskmaster Supervisor          ║");
        println!("║  Type 'help' for available commands   ║");
        println!("╚═══════════════════════════════════════╝\n");
    }

    fn print_help() {
        println!(
            r#"
Available commands:
  start <name>      Start all instances of a program
  stop <name>       Stop a program gracefully
  restart <name>    Restart a program
  status            Show status of all programs
  help              Show this help message
  quit/exit         Shutdown supervisor and exit

Examples:
  taskmaster> start web
  taskmaster> status
  taskmaster> stop api
  taskmaster> restart web
"#
        );
    }

    fn handle_start(supervisor: &Supervisor, name: &str) {
        match supervisor.start_program(name) {
            Ok(_) => println!("✓ Started program: {}", name),
            Err(e) => eprintln!("✗ Failed to start '{}': {:?}", name, e),
        }
    }

    fn handle_stop(supervisor: &Supervisor, name: &str) {
        match supervisor.stop_program(name) {
            Ok(_) => println!("✓ Stopped program: {}", name),
            Err(e) => eprintln!("✗ Failed to stop '{}': {:?}", name, e),
        }
    }

    fn handle_restart(supervisor: &Supervisor, name: &str) {
        match supervisor.stop_program(name) {
            Ok(_) => {
                println!("✓ Stopped program: {}", name);
                std::thread::sleep(std::time::Duration::from_millis(200));
                match supervisor.start_program(name) {
                    Ok(_) => println!("✓ Restarted program: {}", name),
                    Err(e) => eprintln!("✗ Failed to restart '{}': {:?}", name, e),
                }
            }
            Err(e) => eprintln!("✗ Failed to stop '{}': {:?}", name, e),
        }
    }

    fn handle_status(supervisor: &Supervisor) {
        let programs = supervisor.program_names();

        if programs.is_empty() {
            println!("No programs configured.");
            return;
        }

        println!("\n{:<15} {:>10} {:>8}", "Program", "Instances", "Status");
        println!("{}", "-".repeat(50));

        for program in programs {
            let count = supervisor
                .instance_count(&program)
                .unwrap_or(0);
            
            let status = if count > 0 {
                format!("{} running", count)
            } else {
                "STOPPED".to_string()
            };

            println!("{:<15} {:>10} {:>8}", program, count, status);
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_start_command() {
        let cmd = ShellCommand::parse("start web");
        assert!(matches!(cmd, Some(ShellCommand::Start(ref name)) if name == "web"));
    }

    #[test]
    fn parse_stop_command() {
        let cmd = ShellCommand::parse("stop api");
        assert!(matches!(cmd, Some(ShellCommand::Stop(ref name)) if name == "api"));
    }

    #[test]
    fn parse_restart_command() {
        let cmd = ShellCommand::parse("restart database");
        assert!(matches!(cmd, Some(ShellCommand::Restart(ref name)) if name == "database"));
    }

    #[test]
    fn parse_status_command() {
        let cmd = ShellCommand::parse("status");
        assert!(matches!(cmd, Some(ShellCommand::Status)));
    }

    #[test]
    fn parse_quit_command() {
        assert!(matches!(ShellCommand::parse("quit"), Some(ShellCommand::Quit)));
        assert!(matches!(ShellCommand::parse("exit"), Some(ShellCommand::Quit)));
        assert!(matches!(ShellCommand::parse("q"), Some(ShellCommand::Quit)));
    }

    #[test]
    fn parse_help_command() {
        assert!(matches!(ShellCommand::parse("help"), Some(ShellCommand::Help)));
        assert!(matches!(ShellCommand::parse("h"), Some(ShellCommand::Help)));
        assert!(matches!(ShellCommand::parse("?"), Some(ShellCommand::Help)));
    }

    #[test]
    fn parse_empty_line() {
        let cmd = ShellCommand::parse("");
        assert!(cmd.is_none());
        let cmd = ShellCommand::parse("   ");
        assert!(cmd.is_none());
    }

    #[test]
    fn parse_unknown_command() {
        let cmd = ShellCommand::parse("foobar");
        assert!(cmd.is_none());
    }

    #[test]
    fn parse_command_missing_arg() {
        let cmd = ShellCommand::parse("start");
        assert!(cmd.is_none());
    }

    #[test]
    fn parse_with_extra_whitespace() {
        let cmd = ShellCommand::parse("  start   web  ");
        assert!(matches!(cmd, Some(ShellCommand::Start(ref name)) if name == "web"));
    }
}
