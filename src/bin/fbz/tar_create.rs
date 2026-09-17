use std::io::Write;

use fbz::{Bzip2EncodeReport, Bzip2Encoder, EncodeOptions, Error, Result, gzip, lz4, zip::PathInput};

fn append_inputs<W: Write>(inputs: &[PathInput], encoder: W) -> Result<W> {
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);
    for input in inputs {
        archive.append_path_with_name(&input.source, &input.archive_path)?;
    }
    archive.into_inner().map_err(Error::from)
}

pub(super) fn pack_gzip<W: Write + ?Sized>(inputs: &[PathInput], output: &mut W, options: EncodeOptions) -> Result<gzip::EncodeReport> {
    append_inputs(inputs, gzip::Encoder::new(output, options)?)?.finish().map(|(_, report)| report)
}

pub(super) fn pack_lz4<W: Write + ?Sized>(inputs: &[PathInput], output: &mut W, options: EncodeOptions) -> Result<lz4::EncodeReport> {
    append_inputs(inputs, lz4::Encoder::new(output, options)?)?.finish().map(|(_, report)| report)
}

pub(super) fn pack_bzip2<W: Write + ?Sized>(inputs: &[PathInput], output: &mut W, options: EncodeOptions) -> Result<Bzip2EncodeReport> {
    append_inputs(inputs, Bzip2Encoder::new(output, options)?)?.finish().map(|(_, report)| report)
}
