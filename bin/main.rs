use std::env;
use std::path::PathBuf;

use config::model::Config;
use process::Supervisor;
use tui::Shell;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args();
    let executable = arguments.next().unwrap_or_else(|| "taskmaster".to_string());
    let Some(config_argument) = arguments.next() else {
        eprintln!("Usage: {} <config-file>", executable);
        std::process::exit(1);
    };
    if arguments.next().is_some() {
        eprintln!("Usage: {} <config-file>", executable);
        std::process::exit(1);
    }

    let config_path = PathBuf::from(config_argument)
        .canonicalize()
        .map_err(|error| format!("Cannot open configuration file: {}", error))?;
    let config = Config::load_from_path(&config_path)
        .map_err(|error| format!("Failed to load config: {}", error))?;

    std::fs::create_dir_all("./logs")?;
    logger::init_global("./logs/taskmaster.log")?;
    signals::install().map_err(|error| format!("Failed to install signal handlers: {}", error))?;

    println!("Loaded configuration: {}", config_path.display());
    println!(
        "Programs: {}",
        config
            .programs
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let autostart_programs: Vec<String> = config
        .programs
        .iter()
        .filter(|(_, program)| program.autostart)
        .map(|(name, _)| name.clone())
        .collect();
    let mut supervisor = Supervisor::from_config(config);
    for name in autostart_programs {
        if let Err(error) = supervisor.start_program(&name) {
            eprintln!("Autostart {} failed: {}", name, error);
        }
    }

    supervisor.start();
    let shell_result = Shell::run(&supervisor, &config_path);

    println!("Stopping supervised processes...");
    supervisor.stop();
    if !supervisor.wait_for_all_stopped(1) {
        eprintln!("Some supervised processes could not be stopped.");
    }
    println!("Taskmaster stopped.");

    shell_result.map_err(Into::into)
}
