//! Server configuration for performance tuning.

use std::time::Duration;

/// Configuration for the async pipelined server.
///
/// These settings control performance characteristics like batching,
/// buffer sizes, and thread pool configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Capacity of the response channel (pending responses).
    /// Higher values allow more parallelism.
    /// Default: 1024
    pub response_channel_size: usize,

    /// Number of responses to batch before flushing.
    /// Higher values reduce syscalls but increase latency.
    /// Default: 16
    pub write_batch_size: usize,

    /// Maximum time to wait for batch to fill before flushing.
    /// Default: 1ms
    pub write_batch_timeout: Duration,

    /// Size of the write buffer in bytes.
    /// Default: 64KB
    pub write_buffer_size: usize,

    /// Maximum number of blocking threads for filesystem I/O.
    /// Default: 1024
    pub max_blocking_threads: usize,

    /// How long to keep idle blocking threads alive.
    /// Default: 60 seconds
    pub thread_keep_alive: Duration,

    /// Attribute TTL in seconds (for caching).
    /// Default: 1 second
    pub attr_ttl_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            response_channel_size: 1024,
            write_batch_size: 16,
            write_batch_timeout: Duration::from_millis(1),
            write_buffer_size: 64 * 1024,
            max_blocking_threads: 1024,
            thread_keep_alive: Duration::from_secs(60),
            attr_ttl_secs: 1,
        }
    }
}

impl ServerConfig {
    /// Create a new config with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the response channel size.
    pub fn response_channel_size(mut self, size: usize) -> Self {
        self.response_channel_size = size;
        self
    }

    /// Set the write batch size.
    pub fn write_batch_size(mut self, size: usize) -> Self {
        self.write_batch_size = size;
        self
    }

    /// Set the write batch timeout.
    pub fn write_batch_timeout(mut self, timeout: Duration) -> Self {
        self.write_batch_timeout = timeout;
        self
    }

    /// Set the write buffer size.
    pub fn write_buffer_size(mut self, size: usize) -> Self {
        self.write_buffer_size = size;
        self
    }

    /// Set the maximum blocking threads.
    pub fn max_blocking_threads(mut self, count: usize) -> Self {
        self.max_blocking_threads = count;
        self
    }

    /// Set the thread keep-alive duration.
    pub fn thread_keep_alive(mut self, duration: Duration) -> Self {
        self.thread_keep_alive = duration;
        self
    }

    /// Set the attribute TTL.
    pub fn attr_ttl_secs(mut self, secs: u64) -> Self {
        self.attr_ttl_secs = secs;
        self
    }

    /// Configuration optimized for low latency.
    pub fn low_latency() -> Self {
        Self {
            response_channel_size: 256,
            write_batch_size: 1, // Flush immediately
            write_batch_timeout: Duration::from_micros(100),
            write_buffer_size: 16 * 1024,
            max_blocking_threads: 512,
            thread_keep_alive: Duration::from_secs(30),
            attr_ttl_secs: 1,
        }
    }

    /// Configuration optimized for high throughput.
    pub fn high_throughput() -> Self {
        Self {
            response_channel_size: 4096,
            write_batch_size: 64,
            write_batch_timeout: Duration::from_millis(5),
            write_buffer_size: 256 * 1024,
            max_blocking_threads: 2048,
            thread_keep_alive: Duration::from_secs(120),
            attr_ttl_secs: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ServerConfig::default();
        assert_eq!(config.response_channel_size, 1024);
        assert_eq!(config.write_batch_size, 16);
        assert_eq!(config.write_batch_timeout, Duration::from_millis(1));
    }

    #[test]
    fn test_builder_pattern() {
        let config = ServerConfig::new()
            .response_channel_size(2048)
            .write_batch_size(32);

        assert_eq!(config.response_channel_size, 2048);
        assert_eq!(config.write_batch_size, 32);
    }

    #[test]
    fn test_presets() {
        let low_latency = ServerConfig::low_latency();
        assert_eq!(low_latency.write_batch_size, 1);

        let high_throughput = ServerConfig::high_throughput();
        assert_eq!(high_throughput.write_batch_size, 64);
    }
}
