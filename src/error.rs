use std::{error, fmt, io};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError { InvalidMagic, InvalidLevel, Truncated, CrcMismatch, RandomizedBlock, InvalidHuffman, InvalidBlock, BlockOverflow }

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidMagic => "invalid bzip2 magic", Self::InvalidLevel => "invalid bzip2 block size", Self::Truncated => "unexpected end of bzip2 stream",
            Self::CrcMismatch => "bzip2 CRC mismatch", Self::RandomizedBlock => "legacy randomized bzip2 block not supported",
            Self::InvalidHuffman => "invalid bzip2 Huffman table", Self::InvalidBlock => "invalid bzip2 block structure",
            Self::BlockOverflow => "bzip2 block exceeds its declared size"
        })
    }
}

impl error::Error for DecodeError {}

#[derive(Debug)]
pub enum Error {
    InvalidBitCount(u8), InvalidBitOffset { bit_offset: u64, len_bits: u64 }, UnexpectedEof { bit_offset: u64, requested: u64, remaining: u64 },
    InvalidStreamHeader, InvalidGzip(String), InvalidLz4(String), InvalidZip(String), UnsupportedFormat(String),
    Decode { bit_offset: u64, source: DecodeError }, InvalidIndex(String), InvalidConfiguration(String), Io(io::Error)
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBitCount(count) => write!(f, "cannot read {count} bits at once; maximum is 64"),
            Self::InvalidBitOffset { bit_offset, len_bits } => { write!(f, "bit offset {bit_offset} is beyond the {len_bits}-bit input") }
            Self::UnexpectedEof { bit_offset, requested, remaining } => {
                write!(f, "unexpected end of input at bit {bit_offset}: requested {requested} bits, {remaining} remain")
            }
            Self::InvalidStreamHeader => write!(f, "input does not start with a bzip2 BZh1-BZh9 header"),
            Self::InvalidGzip(message) => write!(f, "invalid gzip stream: {message}"),
            Self::InvalidLz4(message) => write!(f, "invalid LZ4 frame: {message}"),
            Self::InvalidZip(message) => write!(f, "invalid ZIP archive: {message}"),
            Self::UnsupportedFormat(message) => f.write_str(message),
            Self::Decode { bit_offset, source } => write!(f, "bzip2 decode error at bit {bit_offset}: {source}"),
            Self::InvalidIndex(message) => write!(f, "invalid fbz index: {message}"),
            Self::InvalidConfiguration(message) => write!(f, "invalid configuration: {message}"),
            Self::Io(source) => source.fmt(f),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self { Self::Decode { source, .. } => Some(source), Self::Io(source) => Some(source), _ => None }
    }
}

impl From<io::Error> for Error { fn from(source: io::Error) -> Self { Self::Io(source) } }

impl Error {
    pub(crate) fn into_io(self) -> io::Error { match self { Self::Io(error) => error, error => io::Error::other(error) } }
}
