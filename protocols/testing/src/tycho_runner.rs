use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use miette::{IntoDiagnostic, WrapErr};
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};
use tycho_simulation::tycho_common::models::Chain;
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

/// An indexer started by [`TychoRunner::start_tycho_streaming`], killed when dropped.
pub struct StreamingIndexer(Option<Child>);

impl StreamingIndexer {
    /// Whether the indexer process has exited. An error checking counts as exited, since the
    /// process can then not be followed either.
    pub fn has_exited(&mut self) -> bool {
        let Some(process) = self.0.as_mut() else {
            return true;
        };
        match process.try_wait() {
            Ok(status) => status.is_some(),
            Err(e) => {
                warn!("Could not check the Tycho indexer process: {e}");
                true
            }
        }
    }
}

impl Drop for StreamingIndexer {
    fn drop(&mut self) {
        if let Some(mut process) = self.0.take() {
            if let Err(e) = process
                .kill()
                .and_then(|()| process.wait().map(|_| ()))
            {
                warn!("Failed to stop the Tycho indexer: {e}");
            }
        }
    }
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
        module_name: Option<&str>,
    ) -> miette::Result<()> {
        info!("Running Tycho indexer from block {start_block} to {end_block}...");
        let mut process = self
            .indexer_command(
                spkg_path,
                start_block,
                end_block,
                protocol_type_names,
                protocol_system,
                module_name,
            )?
            .spawn()
            .into_diagnostic()
            .wrap_err("Error running Tycho indexer")?;

        Self::handle_process_output(&mut process);

        let status = process
            .wait()
            .into_diagnostic()
            .wrap_err("Failed to wait on Tycho indexer process")?;

        // Note: tycho-indexer may exit with non-zero status when stream ends normally
        // This is expected behavior and should not be treated as an error
        if !status.success() {
            debug!("Tycho indexer process exited with status: {status}");
        }

        Ok(())
    }

    /// Starts the indexer for a test that follows the range through its stream, and returns the
    /// running process.
    ///
    /// The indexer waits for the first subscriber before extracting, and delivers every block to
    /// it. It keeps serving after the range: stop it with [`Self::stop_tycho`] once the
    /// subscription has ended. Dropping the returned process without stopping it, as a panic
    /// does, kills it, so it does not hold the server port and the database for the next run.
    pub fn start_tycho_streaming(
        &self,
        spkg_path: &str,
        start_block: u64,
        end_block: u64,
        protocol_type_names: &[String],
        protocol_system: &str,
        module_name: Option<&str>,
    ) -> miette::Result<StreamingIndexer> {
        info!("Running Tycho indexer from block {start_block} to {end_block} for a subscriber...");
        let mut process = self
            .indexer_command(
                spkg_path,
                start_block,
                end_block,
                protocol_type_names,
                protocol_system,
                module_name,
            )?
            .arg("--wait-for-subscriber")
            .spawn()
            .into_diagnostic()
            .wrap_err("Error running Tycho indexer")?;
        Self::handle_process_output(&mut process);
        Ok(StreamingIndexer(Some(process)))
    }

    /// Stops an indexer started by [`Self::start_tycho_streaming`].
    ///
    /// Once its subscription has ended, the extractor has stopped and its commits are written,
    /// so killing the process loses nothing the snapshot reads.
    pub fn stop_tycho(mut indexer: StreamingIndexer) -> miette::Result<()> {
        let Some(mut process) = indexer.0.take() else {
            return Ok(());
        };
        // `kill` succeeds on a process that has already exited.
        process
            .kill()
            .into_diagnostic()
            .wrap_err("Failed to stop the Tycho indexer")?;
        process
            .wait()
            .into_diagnostic()
            .wrap_err("Failed to wait on Tycho indexer process")?;
        Ok(())
    }

    fn indexer_command(
        &self,
        spkg_path: &str,
        start_block: u64,
        end_block: u64,
        protocol_type_names: &[String],
        protocol_system: &str,
        module_name: Option<&str>,
    ) -> miette::Result<Command> {
        let mut cmd = Command::new("tycho-indexer");
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "tycho_indexer=info".to_string()),
        )
        .env("AUTH_API_KEY", "dummy");

        let all_accounts = self.initialized_accounts.clone();

        cmd.args([
            "--database-url",
            self.db_url.as_str(),
            "--server-port",
            &self.server_port.to_string(),
            "--endpoint",
            get_default_endpoint(&self.chain)
                .ok_or_else(|| miette::miette!("Unknown endpoint for chain {}", self.chain))?
                .as_str(),
            "run",
            "--chain",
            self.chain.to_string().as_str(),
            "--spkg",
            spkg_path,
            "--module",
            module_name.unwrap_or("map_protocol_changes"),
            "--protocol-type-names",
            &protocol_type_names.join(","),
            "--protocol-system",
            protocol_system,
            "--start-block",
            &start_block.to_string(),
            "--stop-block",
            // The substreams stop block is exclusive and the indexer leaves the newest streamed
            // block uncommitted, so `+3` commits through `end_block`. It commits `end_block + 1`
            // only when the stream ends slowly: that last commit runs in a task the process does
            // not wait for on exit. The test runner reads the snapshot at the last committed
            // block.
            &(end_block + 3).to_string(),
            "--dci-plugin",
            "rpc",
        ]);

        if !all_accounts.is_empty() {
            cmd.args([
                "--initialized-accounts",
                &all_accounts.join(","),
                "--initialization-block",
                &start_block.to_string(),
            ]);
        }

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd)
    }

    pub fn start_rpc_server(&self) -> TychoRpcServer {
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
                    std::env::var("RUST_LOG").unwrap_or_else(|_| "tycho_indexer=info".to_string()),
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
                    if let Err(e) = cmd.wait() {
                        warn!("Failed to wait for RPC server exit: {e}");
                    }
                }
                Err(_) => {
                    // Channel closed, terminate anyway
                    if let Err(e) = cmd.kill() {
                        warn!("Failed to kill RPC server: {e}");
                    }
                    if let Err(e) = cmd.wait() {
                        warn!("Failed to wait for RPC server exit: {e}");
                    }
                }
            }
        });

        // Give the RPC server time to start
        thread::sleep(Duration::from_secs(3));

        TychoRpcServer { sender: tx, thread_handle }
    }

    pub fn stop_rpc_server(server: TychoRpcServer) -> miette::Result<()> {
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
        protocol_type_names: &[String],
        protocol_system: &str,
        module_name: Option<&str>,
    ) -> miette::Result<()> {
        info!("Running Tycho indexer with Index command (continuous syncing + RPC server) from block {start_block}...");

        // Create temporary extractors.yaml file
        let extractors_config = self.create_extractors_config(
            spkg_path,
            start_block,
            protocol_type_names,
            protocol_system,
            module_name,
        );

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
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "tycho_indexer=info".to_string()),
        )
        .env("AUTH_API_KEY", "dummy");

        cmd.args([
            "--database-url",
            self.db_url.as_str(),
            "--server-port",
            &self.server_port.to_string(),
            "--endpoint",
            get_default_endpoint(&self.chain)
                .ok_or_else(|| miette::miette!("Unknown endpoint for chain {}", self.chain))?
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
        protocol_type_names: &[String],
        protocol_system: &str,
        module_name: Option<&str>,
    ) -> String {
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

        let config = format!(
            "extractors:\n  {}:\n    name: \"{}\"\n    chain: {}\n    implementation_type: Vm\n    sync_batch_size: 1\n    start_block: {}\n    stop_block: null\n    protocol_types:\n{}\n    spkg: \"{}\"\n    module_name: \"{}\"\n{}\n    dci_plugin:\n      type: rpc\n",
            protocol_system,
            protocol_system,
            self.chain.to_string().to_lowercase(),
            start_block,
            protocol_types,
            spkg_path,
            module_name.unwrap_or("map_protocol_changes"),
            initialized_accounts_section
        );

        config
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

pub fn get_default_endpoint(chain: &Chain) -> Option<String> {
    match chain {
        Chain::Ethereum => Some("https://mainnet.eth.streamingfast.io:443".to_string()),
        Chain::Base => Some("https://base-mainnet.streamingfast.io:443".to_string()),
        Chain::Unichain => Some("https://mainnet.unichain.streamingfast.io:443".to_string()),
        Chain::Polygon => Some("https://polygon.streamingfast.io:443".to_string()),
        Chain::Robinhood => Some("https://mainnet.robinhood.streamingfast.io:443".to_string()),
        Chain::Arc => Some("https://arc.substreams.pinax.network:443".to_string()),
        Chain::Bsc => Some("https://bnb.streamingfast.io:443".to_string()),
        Chain::Starknet |
        Chain::ZkSync |
        Chain::Arbitrum |
        Chain::Plasma |
        Chain::Custom(_) |
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_uses_pinax_substreams_endpoint() {
        assert_eq!(
            get_default_endpoint(&Chain::Arc).as_deref(),
            Some("https://arc.substreams.pinax.network:443")
        );
    }

    #[test]
    fn bsc_uses_streamingfast_substreams_endpoint() {
        assert_eq!(
            get_default_endpoint(&Chain::Bsc).as_deref(),
            Some("https://bnb.streamingfast.io:443")
        );
    }
}
