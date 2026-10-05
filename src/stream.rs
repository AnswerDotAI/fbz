use std::{io::Write, path::Path};

use crate::{DecodeOptions, DecodeProgress, Error, OutputSink, Result, WriterSink, gzip, lz4};

/// A byte-stream compression format accepted by the unified decoders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecodeFormat {
    /// Detect the format from magic, falling back to the filename for path inputs.
    #[default]
    Auto,
    Bzip2,
    Gzip,
    Lz4,
}

impl DecodeFormat {
    fn explicit(self) -> Option<Format> {
        match self {
            Self::Auto => None,
            Self::Bzip2 => Some(Format::Bzip2),
            Self::Gzip => Some(Format::Gzip),
            Self::Lz4 => Some(Format::Lz4),
        }
    }

    pub(crate) fn detect_data(self, data: &[u8]) -> Result<Format> {
        self.explicit().or_else(|| Format::from_magic(data)).ok_or_else(|| {
            Error::UnsupportedFormat("cannot determine compression format; expected bzip2, gzip, or LZ4 magic, or an explicit DecodeFormat".into())
        })
    }

    pub fn detect_path(self, path: &Path, data: &[u8]) -> Result<Format> {
        match self.explicit() { Some(format) => Ok(format), None => Format::detect(path, data) }
    }
}

/// A compression or archive format recognized by fbz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Bzip2,
    Gzip,
    Lz4,
    Zip,
}

impl Format {
    /// Detect a format from stream magic, falling back to the path extension.
    pub fn detect(path: impl AsRef<Path>, data: &[u8]) -> Result<Self> {
        if let Some(format) = Self::from_magic(data) { return Ok(format); }
        if let Some(format) = Self::from_path(path.as_ref()) { return Ok(format); }
        Err(Error::UnsupportedFormat(format!(
            "cannot determine compression format for {}; expected a bzip2, gzip, LZ4, or ZIP extension or magic",
            path.as_ref().display()
        )))
    }

    /// Infer a format from a recognized filename extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "bz2" | "bzip2" | "tbz" | "tbz2" => Some(Self::Bzip2),
            "gz" | "gzip" | "tgz" => Some(Self::Gzip),
            "lz4" => Some(Self::Lz4),
            "zip" => Some(Self::Zip),
            _ => None,
        }
    }

    /// Infer a format from its leading magic bytes.
    pub fn from_magic(data: &[u8]) -> Option<Self> {
        if data.starts_with(b"BZh") { Some(Self::Bzip2) } else if data.starts_with(&[0x1f, 0x8b]) { Some(Self::Gzip) } else if data.starts_with(&[0x04, 0x22, 0x4d, 0x18])
            || data.get(..4).is_some_and(|magic| (0x184d_2a50..=0x184d_2a5f).contains(&u32::from_le_bytes(magic.try_into().unwrap())))
        { Some(Self::Lz4) } else if data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06") || data.starts_with(b"PK\x07\x08") { Some(Self::Zip) } else { None }
    }
}

/// Decode a single-stream format through the shared owned-chunk output path.
#[doc(hidden)]
pub fn decode_stream_to_sink_with_progress(
    format: Format,
    data: &[u8],
    output: &mut impl OutputSink,
    options: DecodeOptions,
    progress: impl FnMut(DecodeProgress),
) -> Result<()> {
    match format {
        Format::Bzip2 => crate::decompress_to_sink_with_progress(data, output, options, progress),
        Format::Gzip => gzip::decompress_to_sink_with_options_and_progress(data, output, options, progress).map(|_| ()),
        Format::Lz4 => lz4::decompress_to_sink_with_options_and_progress(data, output, options, progress).map(|_| ()),
        Format::Zip => Err(Error::UnsupportedFormat("ZIP archives do not have one decoded byte stream".into())),
    }
}

/// Decode bzip2, gzip, or LZ4 bytes selected by magic or `options.format`.
pub fn decompress(data: &[u8], options: DecodeOptions) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    decompress_to_writer(data, &mut output, options)?;
    Ok(output)
}

/// Decode bzip2, gzip, or LZ4 bytes to a streaming output.
pub fn decompress_to_writer(data: &[u8], output: &mut impl Write, options: DecodeOptions) -> Result<()> {
    decompress_to_writer_with_progress(data, output, options, |_| {})
}

/// Decode a byte stream and report completed compressed and decoded bytes.
pub fn decompress_to_writer_with_progress(data: &[u8], output: &mut impl Write, options: DecodeOptions, progress: impl FnMut(DecodeProgress)) -> Result<()> {
    let format = options.format.detect_data(data)?;
    let mut output = WriterSink::new(output);
    decode_stream_to_sink_with_progress(format, data, &mut output, options, progress)
}
