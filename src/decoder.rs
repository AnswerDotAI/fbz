use crate::{DecodeError, Error, OutputSink, Result, bz2_crc32, combine_stream_crc};

const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
const END_MAGIC: u64 = 0x1772_4538_5090;
const MAX_CODE_LEN: usize = 20;
const GROUP_SIZE: usize = 50;
const LUT_BITS: u8 = 12;
const LUT_SIZE: usize = 1 << LUT_BITS;

struct Bits<'a> { data: &'a [u8], bit: usize }

impl<'a> Bits<'a> {
    fn at(data: &'a [u8], bit: u64) -> Result<Self> {
        let bit = usize::try_from(bit).map_err(|_| Error::Decode { bit_offset: u64::MAX, source: DecodeError::Truncated })?;
        if bit > data.len().saturating_mul(8) { return Err(Error::Decode { bit_offset: bit as u64, source: DecodeError::Truncated }); }
        Ok(Self { data, bit })
    }

    #[inline]
    fn position(&self) -> u64 { self.bit as u64 }

    #[inline]
    fn remaining(&self) -> usize { self.data.len().saturating_mul(8).saturating_sub(self.bit) }

    #[inline]
    fn peek(&self, count: u8) -> Result<u32> {
        let count = usize::from(count);
        if count > self.remaining() { return Err(Error::Decode { bit_offset: self.position(), source: DecodeError::Truncated }); }
        let byte = self.bit >> 3;
        let offset = self.bit & 7;
        if byte + 8 <= self.data.len() {
            let word = u64::from_be_bytes(self.data[byte..byte + 8].try_into().unwrap());
            return Ok(((word << offset) >> (64 - count)) as u32);
        }
        let stop = (self.bit + count).div_ceil(8);
        let mut word = 0_u64;
        for &value in &self.data[byte..stop] { word = (word << 8) | u64::from(value); }
        let available = (stop - byte) * 8;
        Ok(((word >> (available - offset - count)) & ((1_u64 << count) - 1)) as u32)
    }

    #[inline]
    fn read(&mut self, count: u8) -> Result<u32> {
        if count == 0 { return Ok(0); }
        let value = self.peek(count)?;
        self.bit += usize::from(count);
        Ok(value)
    }

    #[inline]
    fn bit(&mut self) -> Result<bool> { Ok(self.read(1)? != 0) }

    #[inline]
    fn skip(&mut self, count: u8) { self.bit += usize::from(count); }

    #[inline]
    fn magic(&mut self) -> Result<u64> { Ok((u64::from(self.read(24)?) << 24) | u64::from(self.read(24)?)) }

    fn align_byte(&mut self) { self.bit = (self.bit + 7) & !7; }
}

struct Huffman {
    min_len: u8,
    max_len: u8,
    limit: [i32; MAX_CODE_LEN + 1],
    base: [i32; MAX_CODE_LEN + 1],
    symbols: Vec<u16>,
    lut: Box<[u32; LUT_SIZE]>,
}

impl Huffman {
    fn build(lengths: &[u8], bit_offset: u64) -> Result<Self> {
        let min_len = *lengths.iter().min().ok_or_else(|| decode_error(bit_offset, DecodeError::InvalidHuffman))?;
        let max_len = *lengths.iter().max().unwrap();
        if min_len == 0 || usize::from(max_len) > MAX_CODE_LEN { return Err(decode_error(bit_offset, DecodeError::InvalidHuffman)); }

        let mut counts = [0_u32; MAX_CODE_LEN + 1];
        for &length in lengths { counts[usize::from(length)] += 1; }
        let mut next_code = [0_u32; MAX_CODE_LEN + 1];
        let mut code = 0_u32;
        for length in 1..=MAX_CODE_LEN {
            code = (code + counts[length - 1]) << 1;
            next_code[length] = code;
            if code + counts[length] > 1_u32 << length { return Err(decode_error(bit_offset, DecodeError::InvalidHuffman)); }
        }

        let mut lut = Box::new([0_u32; LUT_SIZE]);
        for (symbol, &length) in lengths.iter().enumerate() {
            let canonical = next_code[usize::from(length)];
            next_code[usize::from(length)] += 1;
            if length <= LUT_BITS {
                let fill = 1_usize << (LUT_BITS - length);
                let start = canonical as usize * fill;
                let entry = (u32::from(length) << 16) | symbol as u32;
                lut[start..start + fill].fill(entry);
            }
        }

        let mut symbols = Vec::with_capacity(lengths.len());
        for length in min_len..=max_len {
            symbols.extend(lengths.iter().enumerate().filter_map(|(symbol, &value)| (value == length).then_some(symbol as u16)));
        }

        let mut base = [0_i32; MAX_CODE_LEN + 1];
        for &length in lengths {
            if usize::from(length) == MAX_CODE_LEN { continue; }
            base[usize::from(length) + 1] += 1;
        }
        for index in 1..base.len() { base[index] += base[index - 1]; }
        let mut limit = [0_i32; MAX_CODE_LEN + 1];
        let mut value = 0_i32;
        for length in usize::from(min_len)..=usize::from(max_len) {
            value += counts[length] as i32;
            limit[length] = value - 1;
            value <<= 1;
        }
        for length in usize::from(min_len) + 1..=usize::from(max_len) { base[length] = ((limit[length - 1] + 1) << 1) - base[length]; }
        Ok(Self { min_len, max_len, limit, base, symbols, lut })
    }

    #[inline]
    fn decode(&self, bits: &mut Bits<'_>) -> Result<usize> {
        if bits.remaining() >= usize::from(LUT_BITS) {
            let entry = self.lut[bits.peek(LUT_BITS)? as usize];
            let length = (entry >> 16) as u8;
            if length != 0 {
                bits.skip(length);
                return Ok((entry & 0xffff) as usize);
            }
        }

        let mut length = self.min_len;
        let mut code = bits.read(length)? as i32;
        loop {
            if code <= self.limit[usize::from(length)] {
                let index = code - self.base[usize::from(length)];
                if index >= 0
                    && let Some(&symbol) = self.symbols.get(index as usize)
                { return Ok(usize::from(symbol)); }
                return Err(decode_error(bits.position(), DecodeError::InvalidHuffman));
            }
            if length == self.max_len { return Err(decode_error(bits.position(), DecodeError::InvalidHuffman)); }
            length += 1;
            code = (code << 1) | bits.read(1)? as i32;
        }
    }
}

struct Decoder { tt: Vec<u32> }

pub(crate) struct DecodedCandidate {
    pub output: Vec<u8>,
    pub decoded_len: usize,
    pub end_bit: u64,
    pub block_len: usize,
}

impl Decoder {
    fn new() -> Self { Self { tt: Vec::new() } }

    fn block(&mut self, bits: &mut Bits<'_>, level: u8, expected_crc: Option<u32>) -> Result<(Vec<u8>, u32, usize)> {
        let block_offset = bits.position().saturating_sub(48);
        let stored_crc = bits.read(32)?;
        if expected_crc.is_some_and(|expected| expected != stored_crc) { return Err(decode_error(block_offset, DecodeError::InvalidBlock)); }
        if bits.bit()? { return Err(decode_error(block_offset, DecodeError::RandomizedBlock)); }
        let origin = bits.read(24)? as usize;

        let mut used = [false; 256];
        let groups = bits.read(16)?;
        for group in 0..16 {
            if groups & (1 << (15 - group)) != 0 {
                let values = bits.read(16)?;
                for value in 0..16 { used[group * 16 + value] = values & (1 << (15 - value)) != 0; }
            }
        }
        let alphabet: Vec<u8> = (0..256).filter(|&value| used[value]).map(|value| value as u8).collect();
        if alphabet.is_empty() { return Err(decode_error(block_offset, DecodeError::InvalidBlock)); }
        let alpha_size = alphabet.len() + 2;
        let eob = alpha_size - 1;

        let table_count = bits.read(3)? as usize;
        if !(2..=6).contains(&table_count) { return Err(decode_error(bits.position(), DecodeError::InvalidHuffman)); }
        let selector_count = bits.read(15)? as usize;
        if selector_count == 0 { return Err(decode_error(bits.position(), DecodeError::InvalidBlock)); }
        let mut selector_mtf: Vec<u8> = (0..table_count as u8).collect();
        let mut selectors = Vec::with_capacity(selector_count);
        for _ in 0..selector_count {
            let mut index = 0;
            while bits.bit()? { index += 1; if index >= table_count { return Err(decode_error(bits.position(), DecodeError::InvalidBlock)); } }
            let selected = selector_mtf[index];
            selector_mtf.copy_within(0..index, 1);
            selector_mtf[0] = selected;
            selectors.push(selected as usize);
        }

        let mut tables = Vec::with_capacity(table_count);
        for _ in 0..table_count {
            let mut current = bits.read(5)? as i32;
            let mut lengths = vec![0_u8; alpha_size];
            for length in &mut lengths {
                loop {
                    if !(1..=MAX_CODE_LEN as i32).contains(&current) { return Err(decode_error(bits.position(), DecodeError::InvalidHuffman)); }
                    if !bits.bit()? { break; }
                    current += if bits.bit()? { -1 } else { 1 };
                }
                *length = current as u8;
            }
            tables.push(Huffman::build(&lengths, bits.position())?);
        }

        let block_size = usize::from(level) * 100_000;
        self.tt.clear();
        self.tt.reserve(block_size.saturating_sub(self.tt.capacity()));
        let mut counts = [0_u32; 257];
        let mut mtf = alphabet;
        let mut selector = 0;
        let mut group_left = 0;
        let mut table = 0;
        let mut run = 0_u64;
        let mut run_bit = 0_u32;

        loop {
            if group_left == 0 {
                table = *selectors.get(selector).ok_or_else(|| decode_error(bits.position(), DecodeError::InvalidBlock))?;
                selector += 1;
                group_left = GROUP_SIZE;
            }
            group_left -= 1;
            let symbol = tables[table].decode(bits)?;
            if symbol <= 1 {
                run += (symbol as u64 + 1) << run_bit;
                run_bit += 1;
                if run_bit >= 32 || run > block_size as u64 { return Err(decode_error(bits.position(), DecodeError::BlockOverflow)); }
                continue;
            }
            if run != 0 {
                let byte = mtf[0];
                let run = run as usize;
                if self.tt.len() + run > block_size { return Err(decode_error(bits.position(), DecodeError::BlockOverflow)); }
                self.tt.resize(self.tt.len() + run, u32::from(byte));
                counts[usize::from(byte) + 1] += run as u32;
                run_bit = 0;
            }
            run = 0;
            if symbol == eob { break; }
            let index = symbol - 1;
            if index >= mtf.len() || self.tt.len() == block_size { return Err(decode_error(bits.position(), DecodeError::InvalidBlock)); }
            let byte = mtf[index];
            mtf.copy_within(0..index, 1);
            mtf[0] = byte;
            self.tt.push(u32::from(byte));
            counts[usize::from(byte) + 1] += 1;
        }

        let block_len = self.tt.len();
        if block_len == 0 || origin >= block_len { return Err(decode_error(block_offset, DecodeError::InvalidBlock)); }
        for index in 1..counts.len() { counts[index] += counts[index - 1]; }
        for index in 0..block_len {
            let byte = (self.tt[index] & 0xff) as usize;
            let target = counts[byte] as usize;
            self.tt[target] |= (index as u32) << 8;
            counts[byte] += 1;
        }

        let max_output = block_size / 5 * 259 + 4;
        let mut output = Vec::with_capacity(block_len.min(max_output));
        let mut position = self.tt[origin] >> 8;
        let mut previous = None;
        let mut repeated = 0_u8;
        for _ in 0..block_len {
            let entry = self.tt[position as usize];
            let byte = entry as u8;
            position = entry >> 8;
            if repeated == 4 {
                let extra = usize::from(byte);
                if output.len() + extra > max_output { return Err(decode_error(block_offset, DecodeError::BlockOverflow)); }
                output.resize(output.len() + extra, previous.unwrap());
                previous = None;
                repeated = 0;
            } else {
                if output.len() == max_output { return Err(decode_error(block_offset, DecodeError::BlockOverflow)); }
                output.push(byte);
                if previous == Some(byte) { repeated += 1; } else {
                    previous = Some(byte);
                    repeated = 1;
                }
            }
        }
        if bz2_crc32(&output) != stored_crc { return Err(decode_error(block_offset, DecodeError::CrcMismatch)); }
        Ok((output, stored_crc, block_len))
    }
}

pub(crate) fn decode_candidate(data: &[u8], start_bit: u64, expected_crc: u32) -> Result<DecodedCandidate> {
    let mut bits = Bits::at(data, start_bit)?;
    if bits.magic()? != BLOCK_MAGIC { return Err(decode_error(start_bit, DecodeError::InvalidMagic)); }
    let (output, _, block_len) = Decoder::new().block(&mut bits, 9, Some(expected_crc))?;
    let decoded_len = output.len();
    Ok(DecodedCandidate { output, decoded_len, end_bit: bits.position(), block_len })
}

pub(crate) fn decode_first_candidate(data: &[u8]) -> Result<(u32, DecodedCandidate)> {
    let mut bits = Bits::at(data, 32)?;
    if bits.magic()? != BLOCK_MAGIC { return Err(decode_error(32, DecodeError::InvalidMagic)); }
    let (output, expected_crc, block_len) = Decoder::new().block(&mut bits, 9, None)?;
    let decoded_len = output.len();
    Ok((expected_crc, DecodedCandidate { output, decoded_len, end_bit: bits.position(), block_len }))
}

pub(crate) fn decode_block(data: &[u8], start_bit: u64, end_bit: u64, level: u8, expected_crc: u32) -> Result<Vec<u8>> {
    if !(1..=9).contains(&level) || end_bit <= start_bit { return Err(decode_error(start_bit, DecodeError::InvalidBlock)); }
    let mut bits = Bits::at(data, start_bit)?;
    if bits.magic()? != BLOCK_MAGIC { return Err(decode_error(start_bit, DecodeError::InvalidMagic)); }
    let (output, _, _) = Decoder::new().block(&mut bits, level, Some(expected_crc))?;
    if bits.position() != end_bit { return Err(decode_error(bits.position(), DecodeError::InvalidBlock)); }
    Ok(output)
}

pub(crate) fn decode_serial_with_progress(data: &[u8], output: &mut impl OutputSink, progress: &mut impl FnMut(u64, u64)) -> Result<()> {
    let mut bits = Bits::at(data, 0)?;
    let mut decoder = Decoder::new();
    let mut decoded_bytes = 0_u64;
    while bits.remaining() != 0 {
        bits.align_byte();
        if bits.remaining() < 32 { return Err(decode_error(bits.position(), DecodeError::Truncated)); }
        if bits.read(8)? != u32::from(b'B') || bits.read(8)? != u32::from(b'Z') || bits.read(8)? != u32::from(b'h') {
            return Err(decode_error(bits.position().saturating_sub(24), DecodeError::InvalidMagic));
        }
        let level = bits.read(8)? as u8;
        if !(b'1'..=b'9').contains(&level) { return Err(decode_error(bits.position().saturating_sub(8), DecodeError::InvalidLevel)); }
        let level = level - b'0';
        let mut combined_crc = 0_u32;
        loop {
            let marker_offset = bits.position();
            match bits.magic()? {
                BLOCK_MAGIC => {
                    let (block, crc, _) = decoder.block(&mut bits, level, None)?;
                    let block_len = block.len() as u64;
                    output.write_owned_from(block, 0)?;
                    decoded_bytes = decoded_bytes.checked_add(block_len).ok_or_else(|| Error::InvalidConfiguration("decoded offset overflow".into()))?;
                    progress(bits.position().div_ceil(8), decoded_bytes);
                    combined_crc = combine_stream_crc(combined_crc, crc);
                }
                END_MAGIC => {
                    if bits.read(32)? != combined_crc { return Err(decode_error(marker_offset, DecodeError::CrcMismatch)); }
                    break;
                }
                _ => return Err(decode_error(marker_offset, DecodeError::InvalidMagic)),
            }
        }
        bits.align_byte();
    }
    output.flush()?;
    progress(data.len() as u64, decoded_bytes);
    Ok(())
}

fn decode_error(bit_offset: u64, source: DecodeError) -> Error { Error::Decode { bit_offset, source } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huffman_lut_and_fallback_decode_all_symbols() {
        let lengths = [2, 2, 3, 3, 3, 3];
        let table = Huffman::build(&lengths, 0).unwrap();
        let data = [0b0001_1001, 0b0111_0111];
        let mut bits = Bits::at(&data, 0).unwrap();
        assert_eq!((0..6).map(|_| table.decode(&mut bits).unwrap()).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5]);
    }
}
