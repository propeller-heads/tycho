use std::{fs, panic, path::Path};

use tracing_subscriber::{fmt, EnvFilter};

pub fn setup_tracing() {
    // Archive existing log file if it exists
    archive_existing_log();

    // Create a non-rolling file writer for fresh logs each run
    let log_file =
        std::fs::File::create("logs/price_printer.log").expect("Failed to create log file");

    // Create a subscriber with the file appender
    let subscriber = fmt()
        .with_writer(log_file)
        .with_env_filter(EnvFilter::from_default_env())
        .finish();

    // Set the subscriber as the global default
    tracing::subscriber::set_global_default(subscriber).unwrap();

    // Set up panic hook to log panics
    panic::set_hook(Box::new(|panic_info| {
        let message = if let Some(s) = panic_info
            .payload()
            .downcast_ref::<&str>()
        {
            s.to_string()
        } else if let Some(s) = panic_info
            .payload()
            .downcast_ref::<String>()
        {
            s.clone()
        } else {
            "Unknown panic".to_string()
        };

        tracing::error!(message = %message);
    }));
}

fn archive_existing_log() {
    let log_path = Path::new("logs/price_printer.log");
    let archive_path = Path::new("logs/price_printer_old.log");

    // Create logs directory if it doesn't exist
    if let Some(parent) = log_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("Warning: Failed to create log directory: {e}");
        }
    }

    // If log file exists, move it to the archive
    if log_path.exists() {
        // Remove old archive if it exists
        if archive_path.exists() {
            if let Err(e) = fs::remove_file(archive_path) {
                eprintln!("Warning: Failed to remove old log archive: {e}");
            }
        }

        // Move current log to archive
        if let Err(e) = fs::rename(log_path, archive_path) {
            eprintln!("Warning: Failed to archive existing log: {e}");
        }
    }
}
