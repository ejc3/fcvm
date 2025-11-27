//! Server configuration for performance tuning.

use std::time::Duration;

/// Default batch timeout in microseconds.
///
/// This controls how long we wait for more responses before flushing.
/// Lower = better latency, higher = better throughput under load.
/// 20µs is a good balance given typical operation latency of ~50µs.
const DEFAULT_BATCH_TIMEOUT_US: u64 = 20;

/// Configuration for the async pipelined server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Capacity of the response channel (pending responses).
    pub response_channel_size: usize,

    /// Number of responses to batch before flushing.
    pub write_batch_size: usize,

    /// Maximum time to wait for batch to fill before flushing.
    pub write_batch_timeout: Duration,

    /// Size of the write buffer in bytes.
    pub write_buffer_size: usize,

    /// Maximum number of blocking threads for filesystem I/O.
    pub max_blocking_threads: usize,

    /// How long to keep idle blocking threads alive.
    pub thread_keep_alive: Duration,

    /// Attribute TTL in seconds (for caching).
    pub attr_ttl_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            response_channel_size: 4096,
            write_batch_size: 64,
            write_batch_timeout: Duration::from_micros(DEFAULT_BATCH_TIMEOUT_US),
            write_buffer_size: 256 * 1024,
            max_blocking_threads: 2048,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ServerConfig::default();
        assert_eq!(config.response_channel_size, 4096);
        assert_eq!(config.write_batch_size, 64);
        assert_eq!(config.write_batch_timeout, Duration::from_micros(DEFAULT_BATCH_TIMEOUT_US));
    }

    #[test]
    fn test_builder_pattern() {
        let config = ServerConfig::new()
            .response_channel_size(2048)
            .write_batch_size(32);

        assert_eq!(config.response_channel_size, 2048);
        assert_eq!(config.write_batch_size, 32);
    }
}
