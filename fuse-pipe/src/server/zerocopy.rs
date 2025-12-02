//! ZeroCopy adapters for fuse-backend-rs read/write operations.
//!
//! These adapters bridge our `Vec<u8>`/`&[u8]` protocol with fuse-backend-rs's
//! `ZeroCopyReader`/`ZeroCopyWriter` trait system.

use fuse_backend_rs::api::filesystem::{ZeroCopyReader, ZeroCopyWriter};
use fuse_backend_rs::common::file_buf::FileVolatileSlice;
use fuse_backend_rs::common::file_traits::FileReadWriteVolatile;
use std::io::{self, Read, Write};

/// Collects data read from a file into a `Vec<u8>`.
///
/// Used for read() operations: fuse-backend-rs reads from the file
/// and writes into this adapter, which collects the data.
pub struct VecWriter {
    data: Vec<u8>,
}

impl VecWriter {
    /// Create a new VecWriter with the given capacity hint.
    pub fn new(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
        }
    }

    /// Consume the writer and return the collected data.
    pub fn into_vec(self) -> Vec<u8> {
        self.data
    }
}

impl ZeroCopyWriter for VecWriter {
    fn write_from(
        &mut self,
        f: &mut dyn FileReadWriteVolatile,
        count: usize,
        off: u64,
    ) -> io::Result<usize> {
        let mut buf = vec![0u8; count];
        // SAFETY: buf is valid for count bytes and lives for this call
        let slice = unsafe { FileVolatileSlice::from_raw_ptr(buf.as_mut_ptr(), count) };
        let n = f.read_at_volatile(slice, off)?;
        self.data.extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn available_bytes(&self) -> usize {
        usize::MAX
    }
}

impl Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Provides data from a byte slice to write to a file.
///
/// Used for write() operations: this adapter provides data that
/// fuse-backend-rs reads and writes to the file.
pub struct SliceReader<'a> {
    data: &'a [u8],
}

impl<'a> SliceReader<'a> {
    /// Create a new SliceReader wrapping the given data.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }
}

impl ZeroCopyReader for SliceReader<'_> {
    fn read_to(
        &mut self,
        f: &mut dyn FileReadWriteVolatile,
        count: usize,
        off: u64,
    ) -> io::Result<usize> {
        let to_write = std::cmp::min(count, self.data.len());
        if to_write == 0 {
            return Ok(0);
        }
        // SAFETY: data is valid for to_write bytes and lives for this call.
        // We cast away const because FileVolatileSlice requires *mut, but
        // write_at_volatile only reads from it.
        let slice =
            unsafe { FileVolatileSlice::from_raw_ptr(self.data.as_ptr() as *mut u8, to_write) };
        let n = f.write_at_volatile(slice, off)?;
        self.data = &self.data[n..];
        Ok(n)
    }
}

impl Read for SliceReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = std::cmp::min(buf.len(), self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vec_writer_write_trait() {
        let mut writer = VecWriter::new(16);
        writer.write_all(b"hello").unwrap();
        writer.write_all(b" world").unwrap();
        assert_eq!(writer.into_vec(), b"hello world");
    }

    #[test]
    fn test_slice_reader_read_trait() {
        let data = b"hello world";
        let mut reader = SliceReader::new(data);

        let mut buf = [0u8; 5];
        assert_eq!(reader.read(&mut buf).unwrap(), 5);
        assert_eq!(&buf, b"hello");

        assert_eq!(reader.read(&mut buf).unwrap(), 5);
        assert_eq!(&buf, b" worl");

        assert_eq!(reader.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'd');
    }
}
