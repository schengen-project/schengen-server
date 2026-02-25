//! Schengen Server - A Synergy protocol server using InputCapture portal and libei
//!
//! This application acts as a Synergy server, accepting client connections and
//! forwarding input events through the InputCapture portal using the libei protocol.

use anyhow::{Context, Result};
use clap::Parser;
use log::{info, warn};
use std::collections::HashMap;

mod config;
mod ei;
mod portal;
mod server;

use config::{ClientConfig, Position};

/// Schengen server for accepting Synergy client connections
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Accept client with name and position relative to another client or server
    /// Format: name:position:reference where position is leftof/rightof/topof/bottomof
    /// and reference is another client name or 'self' for the server
    /// Example: --accept laptop:leftof:self --accept desktop:topof:laptop
    #[arg(short, long, value_name = "NAME:POSITION:REFERENCE")]
    accept: Vec<String>,

    /// Port to listen on
    #[arg(short, long, default_value = "24801")]
    port: u16,

    /// Increase verbosity (-v for info, -vv for debug, -vvv for trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

impl Args {
    /// Get the log level based on verbosity count
    fn get_log_level(&self) -> log::LevelFilter {
        match self.verbose {
            0 => log::LevelFilter::Warn,
            1 => log::LevelFilter::Info,
            2 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        }
    }

    /// Parse and validate the client configuration from --accept arguments
    ///
    /// # Returns
    ///
    /// A HashMap mapping client names to their ClientConfig
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The format is invalid
    /// - Position is not one of the valid values
    /// - Multiple clients claim the same position
    /// - A client references a non-existent client
    fn parse_client_config(&self) -> Result<HashMap<String, ClientConfig>> {
        let mut configs = HashMap::new();

        // First pass: parse all configurations
        for accept_str in &self.accept {
            let parts: Vec<&str> = accept_str.split(':').collect();
            if parts.len() != 3 {
                anyhow::bail!(
                    "Invalid --accept format '{}'. Expected format: name:position:reference",
                    accept_str
                );
            }

            let name = parts[0].to_string();
            let position = Position::from_str(parts[1])?;
            let reference = parts[2].to_string();

            if name == "self" {
                anyhow::bail!("Client name 'self' is reserved for the server");
            }

            if configs.contains_key(&name) {
                anyhow::bail!("Duplicate client name: {}", name);
            }

            configs.insert(
                name.clone(),
                ClientConfig {
                    name: name.clone(),
                    position,
                    reference,
                },
            );
        }

        // Second pass: validate all references exist
        for config in configs.values() {
            if config.reference != "self" && !configs.contains_key(&config.reference) {
                anyhow::bail!(
                    "Client '{}' references non-existent client '{}'",
                    config.name,
                    config.reference
                );
            }
        }

        // Third pass: validate no position conflicts
        validate_positions(&configs)?;

        Ok(configs)
    }
}

/// Validate that no two clients claim the same position
///
/// This function checks that no two clients have conflicting positions.
/// A conflict occurs when two clients claim to be in the same position
/// relative to the same reference point.
///
/// # Arguments
///
/// * `configs` - The client configurations to validate
///
/// # Errors
///
/// Returns an error if a position conflict is detected
fn validate_positions(configs: &HashMap<String, ClientConfig>) -> Result<()> {
    // Build a map of (reference, position) -> client_name to detect duplicates
    let mut position_map: HashMap<(String, Position), String> = HashMap::new();

    for config in configs.values() {
        let key = (config.reference.clone(), config.position);
        if let Some(existing_client) = position_map.get(&key) {
            anyhow::bail!(
                "Position conflict: Both '{}' and '{}' claim to be {:?} of '{}'",
                existing_client,
                config.name,
                config.position,
                config.reference
            );
        }
        position_map.insert(key, config.name.clone());
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Detect if we're running under systemd socket activation
    // Check for LISTEN_FDS environment variable that systemd sets
    let is_socket_activated = std::env::var("LISTEN_FDS").is_ok();

    // Use plain logging for systemd (no colors, no timestamps - journald adds those)
    // Use colorful logging for interactive/direct use
    env_logger::Builder::new()
        .filter_level(args.get_log_level())
        .format(move |buf, record| {
            use std::io::Write;

            if is_socket_activated {
                // Plain format for systemd
                writeln!(buf, "{:5} - {}", record.level(), record.args())
            } else {
                // Colorful format with timestamp for interactive use
                const BLUE: &str = "\x1b[34m";
                const GREEN: &str = "\x1b[32m";
                const MAGENTA: &str = "\x1b[35m";
                const RESET: &str = "\x1b[0m";

                let color = if record.target().contains("::server") {
                    BLUE
                } else if record.target().contains("::ei") {
                    GREEN
                } else if record.target().contains("::portal") {
                    MAGENTA
                } else {
                    ""
                };

                writeln!(
                    buf,
                    "{} - {:5} - {}{:30}{} - {}",
                    chrono::Local::now().format("%H:%M:%S"),
                    record.level(),
                    color,
                    record.target(),
                    RESET,
                    record.args()
                )
            }
        })
        .init();

    info!("Starting schengen-server");

    // Parse and validate client configuration
    let client_config = args
        .parse_client_config()
        .context("Failed to parse client configuration")?;

    if client_config.is_empty() {
        warn!("No clients configured. Server will not accept any connections.");
        warn!("Use --accept to configure allowed clients.");
    } else {
        info!("Configured {} client(s):", client_config.len());
        for config in client_config.values() {
            info!(
                "  - '{}' is {:?} of '{}'",
                config.name, config.position, config.reference
            );
        }
    }

    // Step 1: Connect to InputCapture portal and libei
    info!("Step 1/2: Connecting to InputCapture portal and libei...");
    let client_configs_vec: Vec<_> = client_config.values().cloned().collect();
    let input_capture = portal::InputCapturePortal::new(&client_configs_vec).await?;

    // Step 2: Create server instance and attach portal
    let mut server = server::Server::new(client_config);
    server.set_port(args.port);
    let server = server.with_portal(input_capture);

    // Run the server
    server.run().await?;

    info!("Shutting down schengen-server");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_mapping() {
        let args = Args {
            accept: vec![],
            port: 24801,
            verbose: 0,
        };
        assert_eq!(args.get_log_level(), log::LevelFilter::Warn);

        let args = Args {
            accept: vec![],
            port: 24801,
            verbose: 1,
        };
        assert_eq!(args.get_log_level(), log::LevelFilter::Info);

        let args = Args {
            accept: vec![],
            port: 24801,
            verbose: 2,
        };
        assert_eq!(args.get_log_level(), log::LevelFilter::Debug);

        let args = Args {
            accept: vec![],
            port: 24801,
            verbose: 3,
        };
        assert_eq!(args.get_log_level(), log::LevelFilter::Trace);
    }

    #[test]
    fn test_parse_valid_client_config() {
        let args = Args {
            accept: vec![
                "laptop:leftof:self".to_string(),
                "desktop:topof:laptop".to_string(),
            ],
            port: 24801,
            verbose: 0,
        };

        let config = args.parse_client_config().unwrap();
        assert_eq!(config.len(), 2);
        assert!(config.contains_key("laptop"));
        assert!(config.contains_key("desktop"));

        let laptop = &config["laptop"];
        assert_eq!(laptop.name, "laptop");
        assert_eq!(laptop.position, Position::LeftOf);
        assert_eq!(laptop.reference, "self");

        let desktop = &config["desktop"];
        assert_eq!(desktop.name, "desktop");
        assert_eq!(desktop.position, Position::TopOf);
        assert_eq!(desktop.reference, "laptop");
    }

    #[test]
    fn test_parse_invalid_format() {
        let args = Args {
            accept: vec!["laptop:leftof".to_string()],
            port: 24801,
            verbose: 0,
        };

        assert!(args.parse_client_config().is_err());
    }

    #[test]
    fn test_parse_invalid_position() {
        let args = Args {
            accept: vec!["laptop:invalid:self".to_string()],
            port: 24801,
            verbose: 0,
        };

        assert!(args.parse_client_config().is_err());
    }

    #[test]
    fn test_parse_reserved_name() {
        let args = Args {
            accept: vec!["self:leftof:laptop".to_string()],
            port: 24801,
            verbose: 0,
        };

        assert!(args.parse_client_config().is_err());
    }

    #[test]
    fn test_parse_duplicate_name() {
        let args = Args {
            accept: vec![
                "laptop:leftof:self".to_string(),
                "laptop:rightof:self".to_string(),
            ],
            port: 24801,
            verbose: 0,
        };

        assert!(args.parse_client_config().is_err());
    }

    #[test]
    fn test_parse_nonexistent_reference() {
        let args = Args {
            accept: vec!["laptop:leftof:nonexistent".to_string()],
            port: 24801,
            verbose: 0,
        };

        assert!(args.parse_client_config().is_err());
    }

    #[test]
    fn test_parse_position_conflict() {
        let args = Args {
            accept: vec![
                "laptop:leftof:self".to_string(),
                "desktop:leftof:self".to_string(),
            ],
            port: 24801,
            verbose: 0,
        };

        assert!(args.parse_client_config().is_err());
    }

    #[test]
    fn test_parse_no_conflict_different_reference() {
        let args = Args {
            accept: vec![
                "laptop:leftof:self".to_string(),
                "desktop:rightof:self".to_string(),
                "tablet:leftof:desktop".to_string(),
            ],
            port: 24801,
            verbose: 0,
        };

        let config = args.parse_client_config().unwrap();
        assert_eq!(config.len(), 3);
    }
}
