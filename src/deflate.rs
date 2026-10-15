//! Raw DEFLATE compression and decompression shared by gzip and ZIP framing.

use std::io::{Read, Write};

use crate::{DecodeOptions, DecodeProgress, EncodeOptions, OutputSink, Result, deflate_encode, gzip};

pub(crate) const LENGTH_BASE: [usize; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
pub(crate) const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
pub(crate) const DISTANCE_BASE: [usize; 30] =
    [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
pub(crate) const DISTANCE_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

pub use deflate_encode::{EncodeReport, Encoder};
pub use gzip::DeflateReport as Report;

pub fn compress_to_writer(input: &mut impl Read, output: &mut impl Write, options: EncodeOptions) -> Result<EncodeReport> {
    deflate_encode::compress_to_writer(input, output, options)
}

#[doc(hidden)]
pub fn compress_bytes_serial(data: &[u8], level: u8) -> Result<(Vec<u8>, EncodeReport)> {
    EncodeOptions { level: Some(level), ..EncodeOptions::default() }.validate()?;
    Ok(deflate_encode::compress_bytes_serial(data, level))
}

/// Decode one raw DEFLATE stream into an output sink.
#[doc(hidden)]
pub fn decompress_to_sink_with_options_and_progress(
    data: &[u8], output: &mut impl OutputSink, options: DecodeOptions, progress: impl FnMut(DecodeProgress)) -> Result<Report> {
    gzip::decompress_deflate_to_sink_with_options_and_progress(data, output, options, progress)
}
