//! KVM ftrace helper for debugging snapshot restore.
//!
//! Enable with `FCVM_KVM_TRACE=1`. Requires debugfs access (typically root).
//! Captures KVM exit events during VM resume to diagnose issues like
//! triple faults, NPF storms, or unexpected shutdown exits.
//!
//! Output: /tmp/fcvm-kvm-trace-{vm_id}.log

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const TRACING_DIR: &str = "/sys/kernel/debug/tracing";

pub struct KvmTrace {
    vm_id: String,
    output_path: PathBuf,
}

impl KvmTrace {
    /// Start KVM tracing. Enables kvm_exit and kvm_userspace_exit events.
    pub fn start(vm_id: &str) -> Result<Self> {
        let tracing = Path::new(TRACING_DIR);
        if !tracing.exists() {
            anyhow::bail!("debugfs tracing not available at {}", TRACING_DIR);
        }

        // Stop any existing trace
        write_tracing("tracing_on", "0")?;

        // Clear buffer
        write_tracing("trace", "")?;

        // Set buffer size (256MB to handle burst of KVM exits)
        write_tracing("buffer_size_kb", "262144")?;

        // Enable KVM exit events
        enable_event("kvm/kvm_exit", true)?;
        enable_event("kvm/kvm_userspace_exit", true)?;

        // Enable signal events (to catch SIGABRT delivery)
        enable_event("signal/signal_generate", true)?;
        enable_event("signal/signal_deliver", true)?;

        // Start tracing
        write_tracing("tracing_on", "1")?;

        let output_path = PathBuf::from(format!("/tmp/fcvm-kvm-trace-{}.log", vm_id));
        eprintln!(
            "[fcvm] KVM trace started (kvm_exit + signals) → {}",
            output_path.display()
        );

        Ok(Self {
            vm_id: vm_id.to_string(),
            output_path,
        })
    }

    /// Stop tracing and dump captured events to file.
    pub fn stop_and_dump(self) -> Result<String> {
        // Stop tracing
        write_tracing("tracing_on", "0")?;

        // Disable events
        let _ = enable_event("kvm/kvm_exit", false);
        let _ = enable_event("kvm/kvm_userspace_exit", false);
        let _ = enable_event("signal/signal_generate", false);
        let _ = enable_event("signal/signal_deliver", false);

        // Read trace buffer
        let trace_path = Path::new(TRACING_DIR).join("trace");
        let raw = fs::read_to_string(&trace_path).context("reading trace buffer")?;

        // Filter for relevant events (fc_vcpu threads, firecracker signals)
        let mut output = String::new();
        output.push_str(&format!(
            "# KVM trace for VM {} (captured by FCVM_KVM_TRACE)\n",
            self.vm_id
        ));
        output.push_str("#\n");

        let mut kvm_exit_count = 0;
        let mut shutdown_count = 0;
        let mut signal_count = 0;

        for line in raw.lines() {
            if line.starts_with('#') {
                continue;
            }
            let dominated_by_fc =
                line.contains("fc_vcpu") || line.contains("firecracker") || line.contains("fcvm");
            let is_kvm = line.contains("kvm_exit") || line.contains("kvm_userspace_exit");
            let is_signal = line.contains("signal_generate") || line.contains("signal_deliver");

            if dominated_by_fc && (is_kvm || is_signal) {
                output.push_str(line);
                output.push('\n');

                if is_kvm {
                    kvm_exit_count += 1;
                }
                if line.contains("shutdown") || line.contains("SHUTDOWN") {
                    shutdown_count += 1;
                }
                if is_signal {
                    signal_count += 1;
                }
            }
        }

        output.push_str(&format!(
            "\n# Summary: {} KVM exits, {} shutdowns, {} signals\n",
            kvm_exit_count, shutdown_count, signal_count
        ));

        // Write to file
        fs::write(&self.output_path, &output).context("writing KVM trace output")?;

        // Clear trace buffer
        let _ = write_tracing("trace", "");

        let path_str = self.output_path.display().to_string();
        eprintln!(
            "[fcvm] KVM trace: {} exits, {} shutdowns, {} signals → {}",
            kvm_exit_count, shutdown_count, signal_count, path_str
        );

        Ok(path_str)
    }
}

fn write_tracing(file: &str, value: &str) -> Result<()> {
    let path = Path::new(TRACING_DIR).join(file);
    fs::write(&path, value).with_context(|| format!("writing to {}", path.display()))
}

fn enable_event(event: &str, enable: bool) -> Result<()> {
    let path = Path::new(TRACING_DIR)
        .join("events")
        .join(event)
        .join("enable");
    let value = if enable { "1" } else { "0" };
    fs::write(&path, value).with_context(|| format!("enabling event {}", event))
}
