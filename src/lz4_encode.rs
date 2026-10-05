use std::{
    hash::Hasher as _,
    io::{self, Read, Write},
};

use twox_hash::XxHash32;

use crate::{
    EncodeOptions, Error, Result,
    matchfinder::{HashChain, LatestMatch, match_length},
    pipeline::StreamingOrdered,
};

const MAGIC: u32 = 0x184d_2204;
const WINDOW_SIZE: usize = u16::MAX as usize;
const TARGET_BLOCK_SIZE: usize = 4 * 1024 * 1024;
const MIN_BLOCK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeReport { pub input_len: u64, pub output_len: u64, pub blocks: u64 }

fn invalid(message: impl Into<String>) -> Error { Error::InvalidConfiguration(message.into()) }

fn xxhash32(data: &[u8]) -> u32 {
    let mut hasher = XxHash32::with_seed(0);
    hasher.write(data);
    hasher.finish() as u32
}

fn chain_depth(level: u8) -> usize { match level { 1..=6 => 1, 7 => 4, 8 => 16, 9 => 64, _ => unreachable!() } }

fn write_length(output: &mut Vec<u8>, mut length: usize) {
    while length >= 255 {
        output.push(255);
        length -= 255;
    }
    output.push(length as u8);
}

fn write_sequence(output: &mut Vec<u8>, literals: &[u8], distance: usize, match_length: usize) {
    let literal_nibble = literals.len().min(15);
    let match_base = match_length - 4;
    let match_nibble = match_base.min(15);
    output.push(((literal_nibble << 4) | match_nibble) as u8);
    if literals.len() >= 15 { write_length(output, literals.len() - 15); }
    output.extend_from_slice(literals);
    output.extend_from_slice(&(distance as u16).to_le_bytes());
    if match_base >= 15 { write_length(output, match_base - 15); }
}

fn write_last_literals(output: &mut Vec<u8>, literals: &[u8]) {
    let literal_nibble = literals.len().min(15);
    output.push((literal_nibble << 4) as u8);
    if literals.len() >= 15 { write_length(output, literals.len() - 15); }
    output.extend_from_slice(literals);
}

fn compress_block_fast(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len() / 2);
    let mut finder = LatestMatch::new(WINDOW_SIZE);
    let mut anchor = 0;
    let mut position = 0;
    while position + 12 <= input.len() {
        let max_length = input.len() - position - 5;
        let Some(mut candidate) = finder.insert_and_find(input, position) else {
            position += 1 + ((position - anchor) >> 6);
            continue;
        };
        let distance = position - candidate;
        let mut length = match_length(input, position, candidate, max_length);
        while position > anchor && candidate > 0 && input[position - 1] == input[candidate - 1] {
            position -= 1;
            candidate -= 1;
            length += 1;
        }
        write_sequence(&mut output, &input[anchor..position], distance, length);
        let end = position + length;
        if end >= 2 { finder.insert(input, end - 2); }
        position = end;
        anchor = end;
    }
    write_last_literals(&mut output, &input[anchor..]);
    output
}

fn compress_block_high(input: &[u8], level: u8) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len() / 2);
    let max_chain = chain_depth(level);
    let mut finder = HashChain::new(input.len(), WINDOW_SIZE, max_chain);
    let mut anchor = 0;
    let mut position = 0;
    while position + 12 <= input.len() {
        let max_length = input.len() - position - 5;
        let (mut length, distance) = finder.best_match(input, position, max_length, 4, max_chain);
        if length == 0 {
            finder.insert(input, position);
            position += 1;
            continue;
        }
        let mut candidate = position - distance;
        while position > anchor && candidate > 0 && input[position - 1] == input[candidate - 1] {
            position -= 1;
            candidate -= 1;
            length += 1;
        }
        write_sequence(&mut output, &input[anchor..position], distance, length);
        let end = position + length;
        if end >= 2 { finder.insert(input, end - 2); }
        position = end;
        anchor = end;
    }
    write_last_literals(&mut output, &input[anchor..]);
    output
}

fn compress_block(input: &[u8], level: u8) -> Vec<u8> { if level <= 6 { compress_block_fast(input) } else { compress_block_high(input, level) } }

struct Block { bytes: Vec<u8>, input_len: usize, stored: bool }

fn encode_block(input: Vec<u8>, level: u8) -> Block {
    let input_len = input.len();
    let encoded = compress_block(&input, level);
    if encoded.len() < input.len() { Block { bytes: encoded, input_len, stored: false } } else { Block { bytes: input, input_len, stored: true } }
}

fn reservation(block_size: usize) -> usize { block_size.saturating_mul(6).saturating_add((1 << 16) * size_of::<u32>()) }

fn selected_block_size(memory_limit: usize) -> Result<usize> {
    let mut size = TARGET_BLOCK_SIZE;
    while size > MIN_BLOCK_SIZE && reservation(size) > memory_limit { size /= 4; }
    if reservation(size) > memory_limit { return Err(invalid(format!("compression memory limit must be at least {} bytes", reservation(size)))); }
    Ok(size)
}

fn descriptor_code(block_size: usize) -> u8 { match block_size { 64_000..=65_536 => 4, 65_537..=262_144 => 5, 262_145..=1_048_576 => 6, _ => 7 } }

pub struct Encoder<W: Write> {
    output: Option<W>,
    pipeline: StreamingOrdered<Block>,
    options: EncodeOptions,
    buffer: Vec<u8>,
    block_size: usize,
    reservation: usize,
    content_hash: XxHash32,
    input_len: u64,
    output_len: u64,
    blocks: u64,
}

impl<W: Write> Encoder<W> {
    pub fn new(mut output: W, options: EncodeOptions) -> Result<Self> {
        let options = options.validate()?;
        let options = EncodeOptions { level: Some(options.level_or(1)), ..options };
        let block_size = selected_block_size(options.memory_limit)?;
        let reservation = reservation(block_size);
        let workers = options.resolved_threads().min((options.memory_limit / reservation).max(1));
        let flg = 0x64_u8;
        let bd = descriptor_code(block_size) << 4;
        let checksum = (xxhash32(&[flg, bd]) >> 8) as u8;
        output.write_all(&MAGIC.to_le_bytes())?;
        output.write_all(&[flg, bd, checksum])?;
        Ok(Self {
            output: Some(output),
            pipeline: StreamingOrdered::new(workers, options.memory_limit, "fbz-lz4-encode")?,
            options,
            buffer: Vec::with_capacity(block_size),
            block_size,
            reservation,
            content_hash: XxHash32::with_seed(0),
            input_len: 0,
            output_len: 7,
            blocks: 0,
        })
    }

    fn commit_next(&mut self) -> Result<()> {
        let block = self.pipeline.take_next()?;
        let mut size = block.bytes.len() as u32;
        if block.stored { size |= 1 << 31; }
        let output = self.output.as_mut().unwrap();
        output.write_all(&size.to_le_bytes())?;
        output.write_all(&block.bytes)?;
        self.input_len += block.input_len as u64;
        self.output_len += 4 + block.bytes.len() as u64;
        self.blocks += 1;
        Ok(())
    }

    fn submit_buffer(&mut self) -> Result<()> {
        if self.buffer.is_empty() { return Ok(()); }
        while !self.pipeline.can_submit(self.reservation) { self.commit_next()?; }
        let input = std::mem::replace(&mut self.buffer, Vec::with_capacity(self.block_size));
        let level = self.options.level.unwrap();
        self.pipeline.submit(self.reservation, move || encode_block(input, level))
    }

    fn flush_blocks(&mut self) -> Result<()> {
        self.submit_buffer()?;
        while self.pipeline.has_pending() { self.commit_next()?; }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(W, EncodeReport)> {
        self.flush_blocks()?;
        let output = self.output.as_mut().unwrap();
        output.write_all(&0_u32.to_le_bytes())?;
        output.write_all(&(self.content_hash.finish() as u32).to_le_bytes())?;
        output.flush()?;
        self.output_len += 8;
        let report = EncodeReport { input_len: self.input_len, output_len: self.output_len, blocks: self.blocks };
        Ok((self.output.take().unwrap(), report))
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let total = bytes.len();
        self.content_hash.write(bytes);
        while !bytes.is_empty() {
            let take = bytes.len().min(self.block_size - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == self.block_size { self.submit_buffer().map_err(Error::into_io)?; }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> { self.flush_blocks().map_err(Error::into_io)?; self.output.as_mut().unwrap().flush() }
}

pub fn compress(data: &[u8], options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    compress_to_writer(&mut io::Cursor::new(data), &mut output, options)?;
    Ok(output)
}

pub fn compress_to_writer(input: &mut impl Read, output: &mut impl Write, options: EncodeOptions) -> Result<EncodeReport> {
    let mut encoder = Encoder::new(output, options)?;
    io::copy(input, &mut encoder)?;
    let (_, report) = encoder.finish()?;
    Ok(report)
}
