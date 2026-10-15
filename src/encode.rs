use std::{cell::Cell, io::{self, Read, Write}, rc::Rc, thread};

use crate::{Bzip2Encoder, Error, Result, decode::DEFAULT_MEMORY_LIMIT, gzip, lz4};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeFormat { Bzip2, Gzip, Lz4 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeReport { pub format: EncodeFormat, pub input_len: u64, pub output_len: u64 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeProgress { pub input_bytes: u64, pub output_bytes: u64 }

#[derive(Clone, Copy, Debug)]
pub struct EncodeOptions {
    /// Zero selects the process's available parallelism.
    pub threads: usize,
    /// Maximum bytes reserved for in-flight input, working state, and encoded output.
    pub memory_limit: usize,
    /// Format-specific compression level from 1 through 9, or the codec default.
    pub level: Option<u8>,
}

impl Default for EncodeOptions { fn default() -> Self { Self { threads: 0, memory_limit: DEFAULT_MEMORY_LIMIT, level: None } } }

impl EncodeOptions {
    pub fn resolved_threads(self) -> usize { if self.threads != 0 { self.threads } else { thread::available_parallelism().map(usize::from).unwrap_or(1) } }

    pub(crate) fn validate(self) -> Result<Self> {
        if self.level.is_some_and(|level| !(1..=9).contains(&level)) {
            return Err(Error::InvalidConfiguration("compression level must be between 1 and 9".into()));
        }
        if self.memory_limit == 0 { return Err(Error::InvalidConfiguration("compression memory limit must be greater than zero".into())); }
        Ok(self)
    }

    pub(crate) fn level_or(self, default: u8) -> u8 { self.level.unwrap_or(default) }
}

pub enum Encoder<W: Write> { Bzip2(Bzip2Encoder<W>), Gzip(gzip::Encoder<W>), Lz4(lz4::Encoder<W>) }

impl<W: Write> Encoder<W> {
    pub fn new(output: W, format: EncodeFormat, options: EncodeOptions) -> Result<Self> {
        match format {
            EncodeFormat::Bzip2 => Bzip2Encoder::new(output, options).map(Self::Bzip2),
            EncodeFormat::Gzip => gzip::Encoder::new(output, options).map(Self::Gzip), EncodeFormat::Lz4 => lz4::Encoder::new(output, options).map(Self::Lz4)
        }
    }

    pub fn finish(self) -> Result<(W, EncodeReport)> {
        match self {
            Self::Bzip2(encoder) => encoder.finish()
                .map(|(output, report)| (output, EncodeReport { format: EncodeFormat::Bzip2, input_len: report.input_len, output_len: report.output_len })),
            Self::Gzip(encoder) => encoder.finish()
                .map(|(output, report)| (output, EncodeReport { format: EncodeFormat::Gzip, input_len: report.input_len, output_len: report.output_len })),
            Self::Lz4(encoder) => encoder.finish()
                .map(|(output, report)| (output, EncodeReport { format: EncodeFormat::Lz4, input_len: report.input_len, output_len: report.output_len }))
        }
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self { Self::Bzip2(encoder) => encoder.write(bytes), Self::Gzip(encoder) => encoder.write(bytes), Self::Lz4(encoder) => encoder.write(bytes) }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self { Self::Bzip2(encoder) => encoder.flush(), Self::Gzip(encoder) => encoder.flush(), Self::Lz4(encoder) => encoder.flush() }
    }
}

pub fn compress(data: &[u8], format: EncodeFormat, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    compress_to_writer(&mut io::Cursor::new(data), &mut output, format, options)?;
    Ok(output)
}

pub fn compress_to_writer(input: &mut impl Read, output: &mut impl Write, format: EncodeFormat, options: EncodeOptions) -> Result<EncodeReport> {
    compress_to_writer_with_progress(input, output, format, options, |_| {})
}

struct CountingWriter<'a, W> { inner: &'a mut W, written: Rc<Cell<u64>> }

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.written.set(self.written.get() + written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> { self.inner.flush() }
}

pub fn compress_to_writer_with_progress(
    input: &mut impl Read, output: &mut impl Write, format: EncodeFormat, options: EncodeOptions, mut progress: impl FnMut(EncodeProgress)
) -> Result<EncodeReport> {
    let written = Rc::new(Cell::new(0));
    let mut counted = CountingWriter { inner: output, written: Rc::clone(&written) };
    let mut encoder = Encoder::new(&mut counted, format, options)?;
    let mut buffer = vec![0_u8; 128 * 1024];
    let mut input_bytes = 0_u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 { break; }
        encoder.write_all(&buffer[..read])?;
        input_bytes += read as u64;
        progress(EncodeProgress { input_bytes, output_bytes: written.get() });
    }
    let (_, report) = encoder.finish()?;
    progress(EncodeProgress { input_bytes: report.input_len, output_bytes: report.output_len });
    Ok(report)
}
