use std::{cmp, io::{self, Read, Write}, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{Receiver, SyncSender, sync_channel}}};

/// Receives decoded bytes and can take ownership of decoder chunks.
///
/// Implementations that cannot use ownership only need to implement
/// `write_borrowed`. The default `write_owned_from` forwards the suffix.
pub trait OutputSink {
    fn write_borrowed(&mut self, bytes: &[u8]) -> io::Result<()>;

    fn write_owned_from(&mut self, bytes: Vec<u8>, start: usize) -> io::Result<()> {
        let suffix = bytes.get(start..).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "owned chunk start exceeds its length"))?;
        self.write_borrowed(suffix)
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }

    /// Report whether the downstream consumer has stopped accepting output.
    fn is_cancelled(&self) -> bool { false }
}

impl<S: OutputSink + ?Sized> OutputSink for &mut S {
    fn write_borrowed(&mut self, bytes: &[u8]) -> io::Result<()> { (**self).write_borrowed(bytes) }

    fn write_owned_from(&mut self, bytes: Vec<u8>, start: usize) -> io::Result<()> { (**self).write_owned_from(bytes, start) }

    fn flush(&mut self) -> io::Result<()> { (**self).flush() }

    fn is_cancelled(&self) -> bool { (**self).is_cancelled() }
}

/// Adapts any `std::io::Write` destination to an `OutputSink`.
pub struct WriterSink<W> { writer: W }

impl<W> WriterSink<W> { pub fn new(writer: W) -> Self { Self { writer } } }

impl<W: Write> OutputSink for WriterSink<W> {
    fn write_borrowed(&mut self, bytes: &[u8]) -> io::Result<()> { self.writer.write_all(bytes) }

    fn flush(&mut self) -> io::Result<()> { self.writer.flush() }
}

struct Chunk { bytes: Vec<u8>, offset: usize }

/// The producing side of a bounded owned-chunk output pipe.
#[doc(hidden)]
pub struct PipeWriter { sender: SyncSender<Chunk>, cancelled: Arc<AtomicBool> }

impl PipeWriter {
    fn send(&self, bytes: Vec<u8>, offset: usize) -> io::Result<()> {
        let suffix = bytes.get(offset..).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "owned chunk start exceeds its length"))?;
        if suffix.is_empty() { return Ok(()); }
        self.sender.send(Chunk { bytes, offset }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "output reader stopped reading"))
    }
}

impl OutputSink for PipeWriter {
    fn write_borrowed(&mut self, bytes: &[u8]) -> io::Result<()> { self.send(bytes.to_vec(), 0) }

    fn write_owned_from(&mut self, bytes: Vec<u8>, start: usize) -> io::Result<()> { self.send(bytes, start) }

    fn is_cancelled(&self) -> bool { self.cancelled.load(Ordering::Relaxed) }
}

/// The consuming side of a bounded owned-chunk output pipe.
#[doc(hidden)]
pub struct PipeReader { receiver: Receiver<Chunk>, chunk: Vec<u8>, offset: usize, cancelled: Arc<AtomicBool> }

impl Drop for PipeReader { fn drop(&mut self) { self.cancelled.store(true, Ordering::Relaxed); } }

impl Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() { return Ok(0); }
        if self.offset == self.chunk.len() {
            match self.receiver.recv() {
                Ok(chunk) => {
                    self.chunk = chunk.bytes;
                    self.offset = chunk.offset;
                }
                Err(_) => return Ok(0),
            }
        }
        let count = cmp::min(buffer.len(), self.chunk.len() - self.offset);
        buffer[..count].copy_from_slice(&self.chunk[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

/// Create a zero-capacity pipe that transfers owned decoder chunks.
#[doc(hidden)]
pub fn output_pipe() -> (PipeWriter, PipeReader) {
    let (sender, receiver) = sync_channel(0);
    let cancelled = Arc::new(AtomicBool::new(false));
    (PipeWriter { sender, cancelled: Arc::clone(&cancelled) }, PipeReader { receiver, chunk: Vec::new(), offset: 0, cancelled })
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn pipe_transfers_owned_allocation() {
        let (mut writer, mut reader) = output_pipe();
        let bytes = vec![1, 2, 3, 4];
        let pointer = bytes.as_ptr() as usize;
        let worker = thread::spawn(move || writer.write_owned_from(bytes, 1).unwrap());
        let mut output = [0; 3];
        assert_eq!(reader.read(&mut output).unwrap(), 3);
        assert_eq!(output, [2, 3, 4]);
        assert_eq!(reader.chunk.as_ptr() as usize, pointer);
        worker.join().unwrap();
    }
}
