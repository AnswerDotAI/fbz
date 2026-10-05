//! Fast parallel compression and decompression for bzip2, gzip, LZ4, and ZIP.

mod bitreader;
mod block;
mod bz2_encode;
mod crc;
mod decode;
mod decoder;
pub mod deflate;
mod deflate_encode;
mod encode;
mod error;
mod format;
pub mod gzip;
mod history;
mod index;
mod indexed;
pub mod lz4;
mod lz4_encode;
mod matchfinder;
mod output;
mod pipeline;
mod reader;
mod source;
mod stream;
pub mod zip;

pub use bitreader::BitReader;
pub use block::{MAX_DECODED_BLOCK, MAX_ENCODED_BLOCK, decode_block};
pub use bz2_encode::{EncodeReport as Bzip2EncodeReport, Encoder as Bzip2Encoder, compress as compress_bzip2, compress_to_writer as compress_bzip2_to_writer};
pub use crc::{bz2_crc32, combine_stream_crc};
pub use decode::{
    DEFAULT_MEMORY_LIMIT, DecodeOptions, DecodeProgress, build_index, build_index_with_progress, decode_to_writer, decode_to_writer_with_progress,
    decompress as decompress_bzip2, decompress_to_sink_with_progress, decompress_to_writer as decompress_bzip2_to_writer,
    decompress_to_writer_with_progress as decompress_bzip2_to_writer_with_progress,
};
pub use encode::{EncodeFormat, EncodeOptions, EncodeProgress, EncodeReport, Encoder, compress, compress_to_writer, compress_to_writer_with_progress};
pub use error::{DecodeError, Error, Result};
pub use format::{BLOCK_MAGIC, BlockCandidate, END_MAGIC, EndCandidate, ScanResult, StreamHeaderCandidate, scan};
pub use index::{BlockIndex, Index, StreamIndex};
pub use indexed::{DEFAULT_CACHE_LIMIT, IndexedReader};
pub use output::{OutputSink, PipeReader, PipeWriter, WriterSink, output_pipe};
pub use reader::Reader;
pub use source::Source;
pub use stream::{DecodeFormat, Format, decode_stream_to_sink_with_progress, decompress, decompress_to_writer, decompress_to_writer_with_progress};
