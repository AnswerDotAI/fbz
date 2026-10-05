use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    io::{self, Read, Write},
};

use crc32fast::Hasher;

use crate::{
    EncodeOptions, Error, Result,
    deflate::{DISTANCE_BASE, DISTANCE_EXTRA, LENGTH_BASE, LENGTH_EXTRA},
    matchfinder::HashChain,
    pipeline::StreamingOrdered,
};

const WINDOW_SIZE: usize = 32 * 1024;
const TARGET_SEGMENT_SIZE: usize = 1024 * 1024;
const MIN_SEGMENT_SIZE: usize = 64 * 1024;
const MAX_MATCH: usize = 258;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeReport { pub input_len: u64, pub output_len: u64, pub crc: u32 }

struct BitWriter { bytes: Vec<u8>, pending: u64, bits: u8 }

impl BitWriter {
    fn new(capacity: usize) -> Self { Self { bytes: Vec::with_capacity(capacity), pending: 0, bits: 0 } }

    fn write_bits(&mut self, value: u32, count: u8) {
        debug_assert!(count <= 24);
        self.pending |= u64::from(value) << self.bits;
        self.bits += count;
        while self.bits >= 8 {
            self.bytes.push(self.pending as u8);
            self.pending >>= 8;
            self.bits -= 8;
        }
    }

    fn align_zero(&mut self) {
        if self.bits != 0 {
            self.bytes.push(self.pending as u8);
            self.pending = 0;
            self.bits = 0;
        }
    }

    fn write_aligned(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.bits, 0);
        self.bytes.extend_from_slice(bytes);
    }

    fn finish_aligned(mut self) -> Vec<u8> { self.align_zero(); self.bytes }
}

fn reverse_code(code: u16, bits: u8) -> u16 { code.reverse_bits() >> (16 - bits) }

fn fixed_code(symbol: usize) -> (u16, u8) {
    match symbol {
        0..=143 => (reverse_code(0x30 + symbol as u16, 8), 8),
        144..=255 => (reverse_code(0x190 + (symbol - 144) as u16, 9), 9),
        256..=279 => (reverse_code((symbol - 256) as u16, 7), 7),
        280..=287 => (reverse_code(0xc0 + (symbol - 280) as u16, 8), 8),
        _ => unreachable!(),
    }
}

fn write_fixed_symbol(output: &mut BitWriter, symbol: usize) {
    let (code, bits) = fixed_code(symbol);
    output.write_bits(u32::from(code), bits);
}

fn symbol_for(value: usize, bases: &[usize], extras: &[u8]) -> (usize, u32, u8) {
    for index in (0..bases.len()).rev() { if value >= bases[index] { return (index, (value - bases[index]) as u32, extras[index]); } }
    unreachable!()
}

fn write_match(output: &mut BitWriter, length: usize, distance: usize) {
    let (length_index, length_extra, length_bits) = symbol_for(length, &LENGTH_BASE, &LENGTH_EXTRA);
    write_fixed_symbol(output, 257 + length_index);
    output.write_bits(length_extra, length_bits);
    let (distance_symbol, distance_extra, distance_bits) = symbol_for(distance, &DISTANCE_BASE, &DISTANCE_EXTRA);
    output.write_bits(u32::from(reverse_code(distance_symbol as u16, 5)), 5);
    output.write_bits(distance_extra, distance_bits);
}

#[derive(Clone, Copy)]
enum Token { Literal(u8), Match { length: u16, distance: u16 } }

fn chain_depth(level: u8) -> usize { match level { 1 => 4, 2 => 8, 3 => 16, 4 => 32, 5 => 48, 6 => 64, 7 => 96, 8 => 192, 9 => 384, _ => unreachable!() } }

fn sync_boundary(output: &mut BitWriter) {
    output.write_bits(0, 3);
    output.align_zero();
    output.write_aligned(&[0, 0, 0xff, 0xff]);
}

fn tokenize(bytes: &[u8], prefix_len: usize, level: u8) -> Vec<Token> {
    let mut tokens = Vec::with_capacity(bytes.len() - prefix_len);
    let max_chain = chain_depth(level);
    let mut finder = HashChain::new(bytes.len(), WINDOW_SIZE, max_chain);
    let dictionary_start = prefix_len.saturating_sub(WINDOW_SIZE);
    for position in dictionary_start..prefix_len { finder.insert(bytes, position); }
    let mut position = prefix_len;
    while position < bytes.len() {
        let (length, distance) = finder.best_match(bytes, position, MAX_MATCH, 3, max_chain);
        if length == 0 {
            tokens.push(Token::Literal(bytes[position]));
            finder.insert(bytes, position);
            position += 1;
        } else {
            finder.insert(bytes, position);
            if level >= 4 && position + 1 < bytes.len() {
                let (next_length, _) = finder.best_match(bytes, position + 1, MAX_MATCH, 3, max_chain / 2);
                if next_length > length + 1 {
                    tokens.push(Token::Literal(bytes[position]));
                    position += 1;
                    continue;
                }
            }
            tokens.push(Token::Match { length: length as u16, distance: distance as u16 });
            let end = position + length;
            position += 1;
            while position < end {
                finder.insert(bytes, position);
                position += 1;
            }
        }
    }
    tokens
}

fn encode_fixed(tokens: &[Token], input_len: usize) -> Vec<u8> {
    let mut output = BitWriter::new(input_len / 2);
    output.write_bits(0b010, 3);
    for token in tokens {
        match *token {
            Token::Literal(byte) => write_fixed_symbol(&mut output, byte as usize),
            Token::Match { length, distance } => write_match(&mut output, length as usize, distance as usize),
        }
    }
    write_fixed_symbol(&mut output, 256);
    sync_boundary(&mut output);
    output.finish_aligned()
}

fn huffman_lengths(frequencies: &[u32], max_bits: u8) -> Vec<u8> {
    let mut scaled = frequencies.to_vec();
    loop {
        let mut heap = BinaryHeap::new();
        let mut children = vec![None; scaled.len()];
        for (symbol, &frequency) in scaled.iter().enumerate() { if frequency != 0 { heap.push(Reverse((u64::from(frequency), symbol))); } }
        debug_assert!(heap.len() >= 2);
        while heap.len() > 1 {
            let Reverse((left_frequency, left)) = heap.pop().unwrap();
            let Reverse((right_frequency, right)) = heap.pop().unwrap();
            let node = children.len();
            children.push(Some((left, right)));
            heap.push(Reverse((left_frequency + right_frequency, node)));
        }
        let root = heap.pop().unwrap().0.1;
        let mut lengths = vec![0_u8; scaled.len()];
        let mut stack = vec![(root, 0_u8)];
        while let Some((node, depth)) = stack.pop() {
            if node < scaled.len() { lengths[node] = depth.max(1); } else {
                let (left, right) = children[node].unwrap();
                stack.push((left, depth + 1));
                stack.push((right, depth + 1));
            }
        }
        if lengths.iter().copied().max().unwrap_or(0) <= max_bits { return lengths; }
        for frequency in &mut scaled { if *frequency != 0 { *frequency = frequency.div_ceil(2); } }
    }
}

fn ensure_two(frequencies: &mut [u32]) {
    let active: Vec<_> = frequencies.iter().enumerate().filter_map(|(symbol, &frequency)| (frequency != 0).then_some(symbol)).collect();
    if active.len() >= 2 { return; }
    if active.is_empty() {
        frequencies[0] = 1;
        frequencies[1] = 1;
        return;
    }
    let other = if active.first() == Some(&0) { 1 } else { 0 };
    frequencies[other] = 1;
}

fn huffman_codes(lengths: &[u8], max_bits: u8) -> Vec<u16> {
    let mut counts = vec![0_u16; max_bits as usize + 1];
    for &length in lengths { if length != 0 { counts[length as usize] += 1; } }
    let mut next = vec![0_u16; counts.len()];
    let mut code = 0_u16;
    for bits in 1..counts.len() {
        code = (code + counts[bits - 1]) << 1;
        next[bits] = code;
    }
    lengths
        .iter()
        .map(|&length| {
            if length == 0 { 0 } else {
                let code = next[length as usize];
                next[length as usize] += 1;
                reverse_code(code, length)
            }
        })
        .collect()
}

fn frequencies(tokens: &[Token]) -> ([u32; 286], [u32; 30]) {
    let mut literals = [0_u32; 286];
    let mut distances = [0_u32; 30];
    for token in tokens {
        match *token {
            Token::Literal(byte) => literals[byte as usize] += 1,
            Token::Match { length, distance } => {
                let (length_symbol, _, _) = symbol_for(length as usize, &LENGTH_BASE, &LENGTH_EXTRA);
                let (distance_symbol, _, _) = symbol_for(distance as usize, &DISTANCE_BASE, &DISTANCE_EXTRA);
                literals[257 + length_symbol] += 1;
                distances[distance_symbol] += 1;
            }
        }
    }
    literals[256] += 1;
    ensure_two(&mut literals);
    ensure_two(&mut distances);
    (literals, distances)
}

const CODE_LENGTH_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn write_code(output: &mut BitWriter, symbol: usize, codes: &[u16], lengths: &[u8]) { output.write_bits(u32::from(codes[symbol]), lengths[symbol]); }

fn encode_dynamic(tokens: &[Token], input_len: usize) -> Vec<u8> {
    let (literal_frequencies, distance_frequencies) = frequencies(tokens);
    let literal_lengths = huffman_lengths(&literal_frequencies, 15);
    let distance_lengths = huffman_lengths(&distance_frequencies, 15);
    let literal_count = literal_lengths.iter().rposition(|&length| length != 0).unwrap().max(256) + 1;
    let distance_count = distance_lengths.iter().rposition(|&length| length != 0).unwrap() + 1;
    let all_lengths: Vec<_> = literal_lengths[..literal_count].iter().chain(&distance_lengths[..distance_count]).copied().collect();
    let mut code_length_frequencies = [0_u32; 19];
    for &length in &all_lengths { code_length_frequencies[length as usize] += 1; }
    ensure_two(&mut code_length_frequencies);
    let code_length_lengths = huffman_lengths(&code_length_frequencies, 7);
    let code_length_count = CODE_LENGTH_ORDER.iter().rposition(|&symbol| code_length_lengths[symbol] != 0).unwrap().max(3) + 1;
    let literal_codes = huffman_codes(&literal_lengths, 15);
    let distance_codes = huffman_codes(&distance_lengths, 15);
    let code_length_codes = huffman_codes(&code_length_lengths, 7);

    let mut output = BitWriter::new(input_len / 2);
    output.write_bits(0b100, 3);
    output.write_bits((literal_count - 257) as u32, 5);
    output.write_bits((distance_count - 1) as u32, 5);
    output.write_bits((code_length_count - 4) as u32, 4);
    for &symbol in &CODE_LENGTH_ORDER[..code_length_count] { output.write_bits(u32::from(code_length_lengths[symbol]), 3); }
    for &length in &all_lengths { write_code(&mut output, length as usize, &code_length_codes, &code_length_lengths); }
    for token in tokens {
        match *token {
            Token::Literal(byte) => write_code(&mut output, byte as usize, &literal_codes, &literal_lengths),
            Token::Match { length, distance } => {
                let (length_symbol, length_extra, length_bits) = symbol_for(length as usize, &LENGTH_BASE, &LENGTH_EXTRA);
                write_code(&mut output, 257 + length_symbol, &literal_codes, &literal_lengths);
                output.write_bits(length_extra, length_bits);
                let (distance_symbol, distance_extra, distance_bits) = symbol_for(distance as usize, &DISTANCE_BASE, &DISTANCE_EXTRA);
                write_code(&mut output, distance_symbol, &distance_codes, &distance_lengths);
                output.write_bits(distance_extra, distance_bits);
            }
        }
    }
    write_code(&mut output, 256, &literal_codes, &literal_lengths);
    sync_boundary(&mut output);
    output.finish_aligned()
}

fn encode_stored(bytes: &[u8]) -> Vec<u8> {
    let mut output = BitWriter::new(bytes.len() + bytes.len().div_ceil(u16::MAX as usize) * 5);
    for chunk in bytes.chunks(u16::MAX as usize) {
        output.write_bits(0, 3);
        output.align_zero();
        let length = chunk.len() as u16;
        output.write_aligned(&length.to_le_bytes());
        output.write_aligned(&(!length).to_le_bytes());
        output.write_aligned(chunk);
    }
    output.finish_aligned()
}

struct Segment { encoded: Vec<u8>, input_len: usize, crc: Hasher }

fn encode_segment(buffer: Vec<u8>, prefix_len: usize, level: u8) -> Segment {
    let plain = &buffer[prefix_len..];
    let tokens = tokenize(&buffer, prefix_len, level);
    let fixed = encode_fixed(&tokens, plain.len());
    let dynamic = encode_dynamic(&tokens, plain.len());
    let stored = encode_stored(plain);
    let encoded = if dynamic.len() < fixed.len() { dynamic } else { fixed };
    let encoded = if encoded.len() < stored.len() { encoded } else { stored };
    let mut crc = Hasher::new();
    crc.update(plain);
    Segment { encoded, input_len: plain.len(), crc }
}

fn reservation(segment_size: usize) -> usize { (segment_size + WINDOW_SIZE).saturating_mul(6).saturating_add((1 << 16) * size_of::<usize>()) }

fn segment_size(memory_limit: usize) -> Result<usize> {
    let mut size = TARGET_SEGMENT_SIZE;
    while size > MIN_SEGMENT_SIZE && reservation(size) > memory_limit { size /= 2; }
    if reservation(size) > memory_limit {
        return Err(Error::InvalidConfiguration(format!("compression memory limit must be at least {} bytes", reservation(size))));
    }
    Ok(size)
}

pub(crate) fn validate_options(options: EncodeOptions) -> Result<EncodeOptions> {
    let options = options.validate()?;
    segment_size(options.memory_limit)?;
    Ok(options)
}

fn final_block() -> Vec<u8> {
    let mut output = BitWriter::new(5);
    output.write_bits(1, 3);
    output.align_zero();
    output.write_aligned(&[0, 0, 0xff, 0xff]);
    output.finish_aligned()
}

pub(crate) fn compress_bytes_serial(data: &[u8], level: u8) -> (Vec<u8>, EncodeReport) {
    let mut output = Vec::with_capacity(data.len() / 2);
    let mut crc = Hasher::new();
    let mut previous = Vec::new();
    for chunk in data.chunks(TARGET_SEGMENT_SIZE) {
        let mut buffer = Vec::with_capacity(previous.len() + chunk.len());
        buffer.extend_from_slice(&previous);
        buffer.extend_from_slice(chunk);
        let segment = encode_segment(buffer, previous.len(), level);
        output.extend_from_slice(&segment.encoded);
        crc.combine(&segment.crc);
        let keep = chunk.len().min(WINDOW_SIZE);
        if chunk.len() >= WINDOW_SIZE {
            previous.clear();
            previous.extend_from_slice(&chunk[chunk.len() - keep..]);
        } else {
            previous.extend_from_slice(chunk);
            if previous.len() > WINDOW_SIZE {
                let discard = previous.len() - WINDOW_SIZE;
                previous.copy_within(discard.., 0);
                previous.truncate(WINDOW_SIZE);
            }
        }
    }
    output.extend_from_slice(&final_block());
    let report = EncodeReport { input_len: data.len() as u64, output_len: output.len() as u64, crc: crc.finalize() };
    (output, report)
}

pub struct Encoder<W: Write> {
    output: Option<W>,
    options: EncodeOptions,
    pipeline: StreamingOrdered<Result<Segment>>,
    buffer: Vec<u8>,
    prefix_len: usize,
    segment_size: usize,
    reservation: usize,
    crc: Hasher,
    input_len: u64,
    output_len: u64,
}

impl<W: Write> Encoder<W> {
    pub fn new(output: W, options: EncodeOptions) -> Result<Self> {
        let options = validate_options(options)?;
        let options = EncodeOptions { level: Some(options.level_or(6)), ..options };
        let segment_size = segment_size(options.memory_limit)?;
        let reservation = reservation(segment_size);
        let workers = options.resolved_threads().min((options.memory_limit / reservation).max(1));
        Ok(Self {
            output: Some(output),
            options,
            pipeline: StreamingOrdered::new(workers, options.memory_limit, "fbz-deflate")?,
            buffer: Vec::with_capacity(segment_size + WINDOW_SIZE),
            prefix_len: 0,
            segment_size,
            reservation,
            crc: Hasher::new(),
            input_len: 0,
            output_len: 0,
        })
    }

    fn commit_next(&mut self) -> Result<()> {
        let segment = self.pipeline.take_next()??;
        self.output.as_mut().unwrap().write_all(&segment.encoded)?;
        self.crc.combine(&segment.crc);
        self.input_len += segment.input_len as u64;
        self.output_len += segment.encoded.len() as u64;
        Ok(())
    }

    fn submit_buffer(&mut self) -> Result<()> {
        if self.buffer.len() == self.prefix_len { return Ok(()); }
        while !self.pipeline.can_submit(self.reservation) { self.commit_next()?; }
        let mut next = Vec::with_capacity(self.segment_size + WINDOW_SIZE);
        let keep = self.buffer.len().min(WINDOW_SIZE);
        next.extend_from_slice(&self.buffer[self.buffer.len() - keep..]);
        let buffer = std::mem::replace(&mut self.buffer, next);
        let prefix_len = self.prefix_len;
        let level = self.options.level.unwrap();
        self.prefix_len = keep;
        self.pipeline.submit(self.reservation, move || Ok(encode_segment(buffer, prefix_len, level)))
    }

    fn flush_segments(&mut self) -> Result<()> {
        self.submit_buffer()?;
        while self.pipeline.has_pending() { self.commit_next()?; }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(W, EncodeReport)> {
        self.flush_segments()?;
        let final_block = final_block();
        let output = self.output.as_mut().unwrap();
        output.write_all(&final_block)?;
        output.flush()?;
        self.output_len += final_block.len() as u64;
        let report = EncodeReport { input_len: self.input_len, output_len: self.output_len, crc: self.crc.finalize() };
        Ok((self.output.take().unwrap(), report))
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let total = bytes.len();
        while !bytes.is_empty() {
            let used = self.buffer.len() - self.prefix_len;
            let take = bytes.len().min(self.segment_size - used);
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() - self.prefix_len == self.segment_size { self.submit_buffer().map_err(Error::into_io)?; }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> { self.flush_segments().map_err(Error::into_io)?; self.output.as_mut().unwrap().flush() }
}

pub fn compress_to_writer(input: &mut impl Read, output: &mut impl Write, options: EncodeOptions) -> Result<EncodeReport> {
    let mut encoder = Encoder::new(output, options)?;
    io::copy(input, &mut encoder)?;
    let (_, report) = encoder.finish()?;
    Ok(report)
}
