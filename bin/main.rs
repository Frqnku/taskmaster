use std::env;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use config::model::Config;
use logger;
use process::supervisor::Supervisor;
use tui::Shell;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get config file path from command line arguments
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <config-file>", args[0]);
        eprintln!("Example: {} config/example.toml", args[0]);
        std::process::exit(1);
    }

    let config_path = &args[1];

    // Check if config file exists
    if !Path::new(config_path).exists() {
        eprintln!("Error: config file '{}' not found", config_path);
        std::process::exit(1);
    }

    println!("Loading config from: {}", config_path);

    // Load the configuration
    let config = Config::load_from_path(config_path).map_err(|e| {
        format!("Failed to load config: {}", e)
    })?;

    println!("Config loaded successfully!");
    println!("Programs to supervise: {}", config.programs.keys().map(|k| k.as_str()).collect::<Vec<_>>().join(", "));

    // Initialize global logger (fallback for events without per-program logs)
    // Create logs directory if it doesn't exist
    std::fs::create_dir_all("./logs").ok();
    let global_log = "./logs/taskmaster.log";
    logger::init_global(global_log).map_err(|e| {
        format!("Failed to initialize logger: {}", e)
    })?;
    println!("Global logger initialized: {}", global_log);

    // Log each per-program log path
    for (name, program) in &config.programs {
        if let Some(log_path) = &program.log {
            println!("  {} -> {}", name, log_path);
        } else {
            println!("  {} -> (using global fallback)", name);
        }
    }

    // Create supervisor from config
    // Collect autostart programs before moving config
    let autostart_programs: Vec<String> = config.programs
        .iter()
        .filter(|(_, program)| program.autostart)
        .map(|(name, _)| name.clone())
        .collect();

    let mut supervisor = Supervisor::from_config(config);

    // Start programs that have autostart enabled
    for name in autostart_programs {
        if let Err(e) = supervisor.start_program(&name) {
            eprintln!("Warning: Failed to start program '{}': {:?}", name, e);
        }
    }

    // Setup graceful shutdown signal handling
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);
    let signal_notice = Arc::new(AtomicBool::new(false));
    let signal_notice_clone = Arc::clone(&signal_notice);

    // Handle Ctrl+C (SIGINT)
    ctrlc::set_handler(move || {
        if !signal_notice_clone.swap(true, Ordering::SeqCst) {
            println!("\n\nReceived shutdown signal. Stopping supervisor...");
        }
        running_clone.store(false, Ordering::SeqCst);
    }).expect("Error setting Ctrl-C handler");

    // Start the supervisor
    println!("\nStarting supervisor...");
    supervisor.start();
    println!("Supervisor started. Ready for commands.\n");

    // Run the interactive shell in the main thread
    // The shell will manage shutdown when user types 'quit'
    match Shell::run(&supervisor, Arc::clone(&running)) {
        Ok(_) => {
            // User quit the shell, proceed to shutdown
            println!();
        }
        Err(e) => {
            eprintln!("Shell error: {}", e);
        }
    }

    running.store(false, Ordering::SeqCst);

    // Stop the supervisor gracefully
    println!("Stopping supervisor...");
    supervisor.stop();
    
    // Wait for all supervised processes to stop (max 10 seconds)
    println!("Waiting for processes to stop...");
    if supervisor.wait_for_all_stopped(10) {
        println!("All processes stopped cleanly.");
    } else {
        eprintln!("Warning: Some processes did not stop within timeout.");
    }
    
    println!("Supervisor stopped. Exiting.");

    Ok(())
}
