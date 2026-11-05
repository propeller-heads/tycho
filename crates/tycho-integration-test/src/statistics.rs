use std::collections::{HashMap, HashSet};

use tycho_test::execution::models::TychoExecutionResult;

#[derive(Default, Clone)]
pub struct TestStatistics {
    pub blocks_processed: u64,
    pub protocol_statistics: HashMap<String, ProtocolStatistics>,
    pub blocks_seen: HashSet<u64>,
}

#[derive(Default, Clone)]
pub struct ProtocolStatistics {
    pub execution_simulations: u64,
    pub execution_successes: u64,
    pub execution_reverts: u64,
    pub execution_setup_failures: u64,
    pub validation_passed: u64,
    pub validation_failed: u64,
    pub slippage_values: Vec<f64>,
    pub unique_pools: HashSet<String>,
    pub get_limits_success: u64,
    pub get_limits_failure: u64,
    pub get_amount_out_success: u64,
    pub get_amount_out_failure: u64,
}

impl TestStatistics {
    pub fn record_execution_simulation_result(
        &mut self,
        protocol: &str,
        result: &TychoExecutionResult,
    ) {
        let protocol_stat = self
            .protocol_statistics
            .entry(protocol.to_string())
            .or_default();
        protocol_stat.execution_simulations += 1;

        match result {
            TychoExecutionResult::Success { .. } => {
                protocol_stat.execution_successes += 1;
            }
            TychoExecutionResult::Revert { .. } => {
                protocol_stat.execution_reverts += 1;
            }
            TychoExecutionResult::Failed { .. } => {
                protocol_stat.execution_setup_failures += 1;
            }
        }
    }

    pub fn record_execution_slippage(&mut self, protocol: &str, slippage: f64) {
        self.protocol_statistics
            .entry(protocol.to_string())
            .or_default()
            .slippage_values
            .push(slippage);
    }

    pub fn record_pool_tested(&mut self, protocol: &str, component_id: &str) {
        self.protocol_statistics
            .entry(protocol.to_string())
            .or_default()
            .unique_pools
            .insert(component_id.to_string());
    }

    pub fn record_validation_result(&mut self, protocol: &str, passed: bool) {
        let protocol_stat = self
            .protocol_statistics
            .entry(protocol.to_string())
            .or_default();
        if passed {
            protocol_stat.validation_passed += 1;
        } else {
            protocol_stat.validation_failed += 1;
        }
    }

    pub fn record_get_limits(&mut self, protocol: &str, success: bool) {
        let protocol_stat = self
            .protocol_statistics
            .entry(protocol.to_string())
            .or_default();
        if success {
            protocol_stat.get_limits_success += 1;
        } else {
            protocol_stat.get_limits_failure += 1;
        }
    }

    pub fn record_get_amount_out(&mut self, protocol: &str, success: bool) {
        let protocol_stat = self
            .protocol_statistics
            .entry(protocol.to_string())
            .or_default();
        if success {
            protocol_stat.get_amount_out_success += 1;
        } else {
            protocol_stat.get_amount_out_failure += 1;
        }
    }

    /// Count `block_number` toward `blocks_processed`, once: a block already counted (e.g. seen
    /// through another stream) does not count again.
    pub fn record_block_processed(&mut self, block_number: u64) {
        if self.blocks_seen.insert(block_number) {
            self.blocks_processed += 1;
        }
    }

    pub fn print_summary(&self) {
        println!("\n{}", "=".repeat(80));
        println!("INTEGRATION TEST SUMMARY");
        println!("{}", "=".repeat(80));
        println!("\nBlocks Processed: {}", self.blocks_processed);

        if !self.protocol_statistics.is_empty() {
            println!("\nPer-Protocol Statistics:");
            let mut protocols: Vec<_> = self
                .protocol_statistics
                .iter()
                .collect();
            protocols.sort_by_key(|(name, _)| *name);

            for (protocol, stats) in protocols {
                println!("\n  {}:", protocol);
                println!("    Unique Pools: {}", stats.unique_pools.len());
                println!("    Simulations: {}", stats.execution_simulations);

                // Operation statistics
                let total_ops = stats.get_limits_success +
                    stats.get_limits_failure +
                    stats.get_amount_out_success +
                    stats.get_amount_out_failure;
                if total_ops > 0 {
                    println!("    Operations:");
                    println!(
                        "      get_limits: {} success, {} failure",
                        stats.get_limits_success, stats.get_limits_failure
                    );
                    println!(
                        "      get_amount_out: {} success, {} failure",
                        stats.get_amount_out_success, stats.get_amount_out_failure
                    );
                }

                if stats.execution_simulations > 0 {
                    println!("    Simulation Results:");
                    println!(
                        "      Success: {} ({:.1}%)",
                        stats.execution_successes,
                        (stats.execution_successes as f64 / stats.execution_simulations as f64) *
                            100.0
                    );
                    println!(
                        "      Failed: {} ({:.1}%)",
                        stats.execution_reverts + stats.execution_setup_failures,
                        ((stats.execution_reverts + stats.execution_setup_failures) as f64 /
                            stats.execution_simulations as f64) *
                            100.0
                    );
                    println!("        - Reverted: {}", stats.execution_reverts);
                    println!("        - Setup Failures: {}", stats.execution_setup_failures);

                    // Per-protocol slippage stats
                    if !stats.slippage_values.is_empty() {
                        let mut sorted = stats.slippage_values.clone();
                        sorted.sort_by(|a, b| {
                            a.partial_cmp(b)
                                .expect("Failed to compare slippage values")
                        });
                        let count = sorted.len();
                        let sum: f64 = sorted.iter().sum();
                        let avg = sum / count as f64;
                        let min = sorted[0];
                        let max = sorted[count - 1];
                        println!(
                            "      Slippage: avg {:.4}%, min {:.4}%, max {:.4}% ({} samples)",
                            avg, min, max, count
                        );
                    }
                }
                let total_val = stats.validation_passed + stats.validation_failed;
                if total_val > 0 {
                    println!(
                        "    Validations: {} passed, {} failed",
                        stats.validation_passed, stats.validation_failed
                    );
                }
            }
        }

        println!("\n{}", "=".repeat(80));
    }
}

#[cfg(test)]
mod tests {
    use super::TestStatistics;

    #[test]
    fn blocks_are_counted_once_across_streams() {
        let mut stats = TestStatistics::default();
        stats.record_block_processed(100);
        stats.record_block_processed(100); // same block via a second stream
        stats.record_block_processed(101);
        assert_eq!(stats.blocks_processed, 2);
    }
}
