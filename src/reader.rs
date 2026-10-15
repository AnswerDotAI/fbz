use std::{io::{self, Read}, path::Path, sync::mpsc::{Receiver, sync_channel}, thread::{self, JoinHandle}};

use crate::{DecodeOptions, Error, Format, PipeReader, Result, Source, decode_stream_to_sink_with_progress, output_pipe};

#[derive(Clone)]
struct StoredError { kind: io::ErrorKind, message: String }

impl StoredError {
    fn new(error: io::Error) -> Self { Self { kind: error.kind(), message: error.to_string() } }

    fn io_error(&self) -> io::Error { io::Error::new(self.kind, self.message.clone()) }
}

enum State { Reading, Eof, Failed(StoredError) }

/// A streaming, parallel bzip2, gzip, or LZ4 decoder for file-backed streams.
///
/// Successful EOF means the complete stream and its checksums were validated.
/// A corrupt trailer can therefore produce decoded bytes before a later call to
/// `read` returns an error. Dropping before EOF cancels decoding without
/// completing validation.
pub struct Reader { pipe: Option<PipeReader>, result: Option<Receiver<Result<()>>>, worker: Option<JoinHandle<()>>, state: State }

impl Reader {
    /// Open a bzip2, gzip, or LZ4 file and start its decoder coordinator.
    ///
    /// This opens and memory-maps only the compressed source. Format and option
    /// errors are returned here; compressed-data errors are returned later by
    /// `Read::read` as the stream is consumed.
    pub fn open(path: impl AsRef<Path>, options: DecodeOptions) -> Result<Self> {
        let options = options.validate()?;
        let path = path.as_ref();
        let source = Source::open(path)?;
        let format = options.format.detect_path(path, source.as_slice())?;
        if !matches!(format, Format::Bzip2 | Format::Gzip | Format::Lz4) {
            return Err(Error::UnsupportedFormat("fbz::Reader supports bzip2, gzip, and LZ4 streams".into()));
        }

        let (mut output, pipe) = output_pipe();
        let (result_sender, result) = sync_channel(1);
        let worker = thread::Builder::new().name("fbz-reader".into()).spawn(move || {
            let decoded = decode_stream_to_sink_with_progress(format, source.as_slice(), &mut output, options, |_| {});
            drop(output);
            let _ = result_sender.send(decoded);
        }).map_err(Error::from)?;
        Ok(Self { pipe: Some(pipe), result: Some(result), worker: Some(worker), state: State::Reading })
    }

    fn join_worker(&mut self) -> io::Result<()> {
        let Some(worker) = self.worker.take() else { return Ok(()) };
        worker.join().map_err(|_| io::Error::other("fbz decoder worker panicked"))
    }

    fn fail(&mut self, error: io::Error) -> io::Error {
        let error = StoredError::new(error);
        let returned = error.io_error();
        self.state = State::Failed(error);
        returned
    }

    fn finish(&mut self, result: Result<()>) -> io::Result<()> {
        self.pipe.take();
        self.result.take();
        let joined = self.join_worker();
        match result {
            Err(error) => Err(self.fail(read_error(error))),
            Ok(()) => match joined { Ok(()) => { self.state = State::Eof; Ok(()) } Err(error) => Err(self.fail(error)) },
        }
    }

    fn disconnected(&mut self) -> io::Error {
        self.pipe.take();
        self.result.take();
        let error = self.join_worker().err().unwrap_or_else(|| io::Error::other("fbz decoder stopped without reporting completion"));
        self.fail(error)
    }
}

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() { return Ok(0); }
        loop {
            match &self.state { State::Eof => return Ok(0), State::Failed(error) => return Err(error.io_error()), State::Reading => {} }
            let count = self.pipe.as_mut().expect("reading state must own its pipe").read(buffer)?;
            if count != 0 { return Ok(count); }
            let result = match self.result.as_ref().expect("reading state must own its result receiver").recv() {
                Ok(result) => result, Err(_) => return Err(self.disconnected())
            };
            self.finish(result)?;
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.pipe.take();
        self.result.take();
        let _ = self.join_worker();
    }
}

fn read_error(error: Error) -> io::Error {
    match error {
        Error::Io(error) => error, Error::InvalidConfiguration(message) => io::Error::new(io::ErrorKind::InvalidInput, message),
        error => io::Error::new(io::ErrorKind::InvalidData, error)
    }
}
