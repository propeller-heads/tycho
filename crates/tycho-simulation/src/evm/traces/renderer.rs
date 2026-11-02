use std::fmt::Write;

use revm_inspectors::tracing::{types::CallTraceNode, CallTraceArena};

/// Simple trace renderer that formats traces as human-readable text
pub struct TraceRenderer {
    /// Whether to include gas usage information
    include_gas: bool,
    /// Whether to include return data
    include_return_data: bool,
    /// Maximum depth to render (None = unlimited)
    max_depth: Option<usize>,
}

impl TraceRenderer {
    /// Create a new trace renderer with default settings
    pub fn new() -> Self {
        Self { include_gas: true, include_return_data: true, max_depth: None }
    }

    /// Render a trace arena as formatted text
    pub fn render_arena(&self, arena: &CallTraceArena) -> String {
        let mut output = String::new();
        if !arena.nodes().is_empty() {
            self.render_node(&mut output, arena, 0, 0)
                .expect("writing to a String cannot fail");
        }
        output
    }

    /// Render a single node and its children
    fn render_node(
        &self,
        output: &mut String,
        arena: &CallTraceArena,
        node_index: usize,
        depth: usize,
    ) -> std::fmt::Result {
        // Check max depth
        if let Some(max_depth) = self.max_depth {
            if depth >= max_depth {
                return Ok(());
            }
        }

        let nodes = arena.nodes();
        if node_index >= nodes.len() {
            return Ok(());
        }

        let node = &nodes[node_index];

        // Write indentation
        let indent = "  ".repeat(depth);
        write!(output, "{}", indent)?;

        // Render the trace
        self.render_trace(output, node, depth == 0)?;

        // Render children
        for &child_index in &node.children {
            self.render_node(output, arena, child_index, depth + 1)?;
        }
        Ok(())
    }

    /// Render a single trace line
    fn render_trace(
        &self,
        output: &mut String,
        node: &CallTraceNode,
        _is_root: bool,
    ) -> std::fmt::Result {
        let trace = &node.trace;

        // Get the decoded trace if available
        let decoded = trace
            .decoded
            .as_ref()
            .map(|d| d.as_ref());

        // Address and label
        let address_str = if let Some(decoded) = decoded {
            if let Some(label) = &decoded.label {
                format!("{}[{}]", label, trace.address)
            } else {
                format!("{}", trace.address)
            }
        } else {
            format!("{}", trace.address)
        };

        // Call signature
        let call_str = if let Some(decoded) = decoded {
            if let Some(call_data) = &decoded.call_data {
                if call_data.args.is_empty() {
                    call_data.signature.clone()
                } else {
                    format!(
                        "{}({})",
                        call_data
                            .signature
                            .split('(')
                            .next()
                            .unwrap_or(&call_data.signature),
                        call_data.args.join(", ")
                    )
                }
            } else if trace.data.is_empty() {
                "()".to_string()
            } else {
                format!("0x{}", hex::encode(&trace.data))
            }
        } else if trace.data.is_empty() {
            "()".to_string()
        } else {
            format!("0x{}", hex::encode(&trace.data))
        };

        // Build the line
        write!(output, "[{}] {}::{}", trace.depth, address_str, call_str)?;

        // Add gas info if requested
        if self.include_gas {
            write!(output, " [gas: {}]", trace.gas_used)?;
        }

        // Add success/failure indicator
        if trace.success {
            write!(output, " ✓")?;
        } else {
            write!(output, " ✗")?;
        }

        writeln!(output)?;

        // Add return data if requested and available (on a new line with indentation)
        if self.include_return_data {
            if let Some(decoded) = decoded {
                if let Some(return_data) = &decoded.return_data {
                    let return_indent = "  ".repeat(trace.depth + 1);
                    writeln!(output, "{}→ {}", return_indent, return_data)?;
                }
            } else if !trace.output.is_empty() {
                let return_indent = "  ".repeat(trace.depth + 1);
                writeln!(output, "{}→ 0x{}", return_indent, hex::encode(&trace.output))?;
            }
        }

        // Render logs for this trace
        for log in &node.logs {
            Self::render_log(output, log)?;
        }
        Ok(())
    }

    /// Render a log entry
    fn render_log(
        output: &mut String,
        log: &revm_inspectors::tracing::types::CallLog,
    ) -> std::fmt::Result {
        let indent = "    "; // Extra indentation for logs
        write!(output, "{}├─ ", indent)?;

        if let Some(decoded) = &log.decoded {
            if let Some(name) = &decoded.name {
                write!(output, "emit {}(", name)?;
                if let Some(params) = &decoded.params {
                    let param_strs: Vec<String> = params
                        .iter()
                        .map(|(name, value)| format!("{}: {}", name, value))
                        .collect();
                    write!(output, "{}", param_strs.join(", "))?;
                }
                write!(output, ")")?;
            } else {
                write!(output, "emit <unknown>")?;
            }
        } else {
            // Raw log data
            if !log.raw_log.topics().is_empty() {
                write!(output, "emit 0x{}", hex::encode(log.raw_log.topics()[0].as_slice()))?;
                if log.raw_log.topics().len() > 1 {
                    write!(output, " (+ {} topics)", log.raw_log.topics().len() - 1)?;
                }
            } else {
                write!(output, "emit <no topics>")?;
            }
        }

        writeln!(output)
    }
}

impl Default for TraceRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Utility function to render a trace arena as a formatted string
pub fn render_trace_arena(arena: &CallTraceArena) -> String {
    TraceRenderer::new().render_arena(arena)
}
