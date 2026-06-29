use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use miette::{IntoDiagnostic, WrapErr};
use tempfile::NamedTempFile;
use tracing::{debug, info};
use tycho_simulation::tycho_common::models::Chain;

use crate::config::implementation_type_for;

pub struct TychoRunner {
    chain: Chain,
    db_url: String,
    server_port: u16,
    initialized_accounts: Vec<String>,
}

pub struct TychoRpcServer {
    sender: Sender<bool>,
    thread_handle: thread::JoinHandle<()>,
}

impl TychoRunner {
    pub fn new(
        chain: Chain,
        db_url: String,
        server_port: u16,
        initialized_accounts: Vec<String>,
    ) -> Self {
        Self { chain, db_url, server_port, initialized_accounts }
    }

    pub fn run_tycho(
        &self,
        spkg_path: &str,
        start_block: u64,
        end_block: u64,
        protocol_type_names: &[String],
        protocol_system: &str,
        module_name: Option<String>,
    ) -> miette::Result<()> {
        info!("Running Tycho indexer from block {start_block} to {end_block}...");

        self.run_tycho_index(
            spkg_path,
            start_block,
            Some(end_block + 3),
            protocol_type_names,
            protocol_system,
            module_name,
        )
    }

    pub fn start_rpc_server(&self) -> miette::Result<TychoRpcServer> {
        let (tx, rx): (Sender<bool>, Receiver<bool>) = mpsc::channel();
        let db_url = self.db_url.clone();
        let server_port = self.server_port.to_string();
        let chain = self.chain.to_string();

        // Start the RPC server in a separate thread
        let thread_handle = thread::spawn(move || {
            let mut cmd = Command::new("tycho-indexer")
                .args([
                    "--database-url",
                    db_url.as_str(),
                    "--server-port",
                    server_port.as_str(),
                    "rpc",
                    "--chain",
                    chain.as_str(),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env(
                    "RUST_LOG",
                    std::env::var("RUST_LOG").unwrap_or("tycho_indexer=info".to_string()),
                )
                .env("AUTH_API_KEY", "dummy")
                .spawn()
                .expect("Failed to start RPC server");

            Self::handle_process_output(&mut cmd);

            match rx.recv() {
                Ok(_) => {
                    debug!("Received termination message, stopping RPC server...");
                    cmd.kill()
                        .expect("Failed to kill RPC server");
                    let _ = cmd.wait();
                }
                Err(_) => {
                    // Channel closed, terminate anyway
                    let _ = cmd.kill();
                    let _ = cmd.wait();
                }
            }
        });

        // Give the RPC server time to start
        thread::sleep(Duration::from_secs(3));

        Ok(TychoRpcServer { sender: tx, thread_handle })
    }

    pub fn stop_rpc_server(&self, server: TychoRpcServer) -> miette::Result<()> {
        server
            .sender
            .send(true)
            .into_diagnostic()
            .wrap_err("Failed to send termination message")?;

        // Wait for the RPC thread to finish
        if server.thread_handle.join().is_err() {
            eprintln!("Failed to join RPC thread");
        }

        Ok(())
    }

    pub fn run_tycho_index(
        &self,
        spkg_path: &str,
        start_block: u64,
        stop_block: Option<u64>,
        protocol_type_names: &[String],
        protocol_system: &str,
        module_name: Option<String>,
    ) -> miette::Result<()> {
        info!("Running Tycho indexer with Index command (continuous syncing + RPC server) from block {start_block}...");

        // Create temporary extractors.yaml file
        let extractors_config = self.create_extractors_config(
            spkg_path,
            start_block,
            stop_block,
            protocol_type_names,
            protocol_system,
            module_name,
        )?;

        let mut temp_file = NamedTempFile::new()
            .into_diagnostic()
            .wrap_err("Failed to create temporary extractors config file")?;

        temp_file
            .write_all(extractors_config.as_bytes())
            .into_diagnostic()
            .wrap_err("Failed to write extractors config")?;

        let temp_path = temp_file
            .path()
            .to_str()
            .ok_or_else(|| miette::miette!("Invalid temp file path"))?;

        let mut cmd = Command::new("tycho-indexer");
        cmd.env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or("tycho_indexer=info".to_string()))
            .env("AUTH_API_KEY", "dummy");

        cmd.args([
            "--database-url",
            self.db_url.as_str(),
            "--server-port",
            &self.server_port.to_string(),
            "--endpoint",
            get_default_endpoint(&self.chain)
                .unwrap_or_else(|| panic!("Unknown endpoint for chain {}", self.chain))
                .as_str(),
            "index",
            "--extractors-config",
            temp_path,
            "--chains",
            &self.chain.to_string(),
            "--retention-horizon",
            "2099-01-01T00:00:00",
        ]);

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut process = cmd
            .spawn()
            .into_diagnostic()
            .wrap_err("Error running Tycho indexer with Index command")?;

        Self::handle_process_output(&mut process);

        // Keep the temp file alive until process finishes
        let _temp_file_guard = temp_file;

        let status = process
            .wait()
            .into_diagnostic()
            .wrap_err("Failed to wait on Tycho indexer process")?;

        if !status.success() {
            debug!("Tycho indexer Index process exited with status: {status}");
        }

        Ok(())
    }

    fn create_extractors_config(
        &self,
        spkg_path: &str,
        start_block: u64,
        stop_block: Option<u64>,
        protocol_type_names: &[String],
        protocol_system: &str,
        module_name: Option<String>,
    ) -> miette::Result<String> {
        let protocol_types = protocol_type_names
            .iter()
            .map(|name| format!("    - name: \"{}\"\n      financial_type: Swap", name))
            .collect::<Vec<_>>()
            .join("\n");

        let initialized_accounts_section = if self.initialized_accounts.is_empty() {
            String::new()
        } else {
            format!(
                "    initialized_accounts:\n{}\n    initialized_accounts_block: {}",
                self.initialized_accounts
                    .iter()
                    .map(|acc| format!("      - \"{}\"", acc))
                    .collect::<Vec<_>>()
                    .join("\n"),
                start_block
            )
        };

        let stop_block = stop_block.map_or("null".to_string(), |block| block.to_string());
        let config = format!(
            "extractors:\n  {}:\n    name: \"{}\"\n    chain: {}\n    implementation_type: {}\n    sync_batch_size: 1\n    start_block: {}\n    stop_block: {}\n    protocol_types:\n{}\n    spkg: \"{}\"\n    module_name: \"{}\"\n{}\n    dci_plugin:\n      type: rpc\n",
            protocol_system,
            protocol_system,
            self.chain.to_string().to_lowercase(),
            implementation_type_for(protocol_system),
            start_block,
            stop_block,
            protocol_types,
            spkg_path,
            module_name
                .as_deref()
                .unwrap_or("map_protocol_changes"),
            initialized_accounts_section
        );

        Ok(config)
    }

    // Helper method to handle process output in separate threads
    fn handle_process_output(child: &mut Child) {
        if let Some(stdout) = child.stdout.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    println!("{line}");
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    eprintln!("{line}");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_yaml::Value;
    use tycho_simulation::tycho_common::models::Chain;

    use super::*;

    #[test]
    fn generated_extractor_config_derives_implementation_type() {
        let runner = TychoRunner::new(Chain::Ethereum, "postgres://test".to_string(), vec![]);

        let config = runner
            .create_extractors_config(
                "baseline.spkg",
                24929812,
                None,
                &["baseline".to_string()],
                "baseline",
                Some("map_protocol_changes".to_string()),
            )
            .unwrap();

        let parsed: Value = serde_yaml::from_str(&config).unwrap();
        assert_eq!(
            parsed["extractors"]["baseline"]["implementation_type"],
            Value::String("Custom".to_string())
        );
        assert_eq!(parsed["extractors"]["baseline"]["stop_block"], Value::Null);

        let config = runner
            .create_extractors_config(
                "baseline.spkg",
                24929812,
                Some(24929820),
                &["baseline".to_string()],
                "baseline",
                Some("map_protocol_changes".to_string()),
            )
            .unwrap();

        let parsed: Value = serde_yaml::from_str(&config).unwrap();
        assert_eq!(parsed["extractors"]["baseline"]["stop_block"], Value::Number(24929820.into()));

        let config = runner
            .create_extractors_config(
                "curve.spkg",
                1,
                None,
                &["curve_pool".to_string()],
                "vm:curve",
                None,
            )
            .unwrap();

        let parsed: Value = serde_yaml::from_str(&config).unwrap();
        assert_eq!(
            parsed["extractors"]["vm:curve"]["implementation_type"],
            Value::String("Vm".to_string())
        );
    }
}

pub fn get_default_endpoint(chain: &Chain) -> Option<String> {
    match chain {
        Chain::Ethereum => Some("https://mainnet.eth.streamingfast.io:443".to_string()),
        Chain::Base => Some("https://base-mainnet.streamingfast.io:443".to_string()),
        Chain::Unichain => Some("https://mainnet.unichain.streamingfast.io:443".to_string()),
        Chain::Polygon => Some("https://polygon.streamingfast.io:443".to_string()),
        _ => None,
    }
}
