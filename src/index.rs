use std::{fs, path::Path};

use crate::{Error, Result};

const MAGIC: &[u8; 8] = b"FBZ2IDX\0";
const VERSION: u32 = 1;
const CHECKSUM_LEN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockIndex {
    pub compressed_start_bit: u64, pub compressed_end_bit: u64, pub decoded_start: u64, pub decoded_len: u64, pub expected_crc: u32, pub stream: u64
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamIndex {
    pub compressed_header_byte: u64, pub block_size_100k: u8, pub first_block: u64, pub block_count: u64, pub decoded_start: u64, pub decoded_len: u64,
    pub eos_bit: u64, pub expected_stream_crc: u32
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index { pub source_len: u64, pub source_hash: [u8; 32], pub decoded_len: u64, pub streams: Vec<StreamIndex>, pub blocks: Vec<BlockIndex> }

impl Index {
    pub(crate) fn new(source: &[u8], decoded_len: u64, streams: Vec<StreamIndex>, blocks: Vec<BlockIndex>) -> Self {
        Self { source_len: source.len() as u64, source_hash: *blake3::hash(source).as_bytes(), decoded_len, streams, blocks }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(76 + self.streams.len() * 53 + self.blocks.len() * 44 + CHECKSUM_LEN);
        out.extend_from_slice(MAGIC);
        put_u32(&mut out, VERSION);
        put_u64(&mut out, self.source_len);
        out.extend_from_slice(&self.source_hash);
        put_u64(&mut out, self.decoded_len);
        put_u64(&mut out, self.streams.len() as u64);
        put_u64(&mut out, self.blocks.len() as u64);
        for stream in &self.streams {
            put_u64(&mut out, stream.compressed_header_byte);
            out.push(stream.block_size_100k);
            put_u64(&mut out, stream.first_block);
            put_u64(&mut out, stream.block_count);
            put_u64(&mut out, stream.decoded_start);
            put_u64(&mut out, stream.decoded_len);
            put_u64(&mut out, stream.eos_bit);
            put_u32(&mut out, stream.expected_stream_crc);
        }
        for block in &self.blocks {
            put_u64(&mut out, block.compressed_start_bit);
            put_u64(&mut out, block.compressed_end_bit);
            put_u64(&mut out, block.decoded_start);
            put_u64(&mut out, block.decoded_len);
            put_u32(&mut out, block.expected_crc);
            put_u64(&mut out, block.stream);
        }
        let checksum = blake3::hash(&out);
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    pub fn from_bytes(encoded: &[u8], source: &[u8]) -> Result<Self> {
        if encoded.len() < CHECKSUM_LEN { return Err(invalid("truncated header")); }
        let (payload, checksum) = encoded.split_at(encoded.len() - CHECKSUM_LEN);
        if blake3::hash(payload).as_bytes() != checksum { return Err(invalid("payload checksum mismatch")); }
        let mut reader = IndexReader::new(payload);
        if reader.take(8)? != MAGIC { return Err(invalid("bad magic")); }
        if reader.u32()? != VERSION { return Err(invalid("unsupported version")); }
        let source_len = reader.u64()?;
        let source_hash: [u8; 32] = reader.take(32)?.try_into().unwrap();
        let decoded_len = reader.u64()?;
        let stream_count = reader.usize("stream count")?;
        let block_count = reader.usize("block count")?;
        if source_len != source.len() as u64 || source_hash != *blake3::hash(source).as_bytes() { return Err(invalid("source identity mismatch")); }
        let required = stream_count.checked_mul(53).and_then(|size| block_count.checked_mul(44).and_then(|blocks| size.checked_add(blocks)))
            .ok_or_else(|| invalid("record counts overflow"))?;
        if reader.remaining() != required { return Err(invalid("record counts do not match payload length")); }

        let mut streams = Vec::with_capacity(stream_count);
        for _ in 0..stream_count {
            streams.push(StreamIndex { compressed_header_byte: reader.u64()?, block_size_100k: reader.byte()?, first_block: reader.u64()?,
                block_count: reader.u64()?, decoded_start: reader.u64()?, decoded_len: reader.u64()?, eos_bit: reader.u64()?, expected_stream_crc: reader.u32()?
            });
        }
        let mut blocks = Vec::with_capacity(block_count);
        for _ in 0..block_count {
            blocks.push(BlockIndex { compressed_start_bit: reader.u64()?, compressed_end_bit: reader.u64()?, decoded_start: reader.u64()?,
                decoded_len: reader.u64()?, expected_crc: reader.u32()?, stream: reader.u64()? });
        }
        let index = Self { source_len, source_hash, decoded_len, streams, blocks };
        index.validate()?;
        Ok(index)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> { fs::write(path, self.to_bytes()).map_err(Error::from) }

    pub fn load(path: impl AsRef<Path>, source: &[u8]) -> Result<Self> { Self::from_bytes(&fs::read(path)?, source) }

    fn validate(&self) -> Result<()> {
        let source_bits = self.source_len.checked_mul(8).ok_or_else(|| invalid("source length overflow"))?;
        let mut decoded = 0_u64;
        for (number, block) in self.blocks.iter().enumerate() {
            if block.decoded_start != decoded || block.decoded_len == 0 { return Err(invalid("block decoded offsets are not contiguous")); }
            if block.compressed_start_bit >= block.compressed_end_bit || block.compressed_end_bit > source_bits {
                return Err(invalid("block compressed range is invalid"));
            }
            if block.stream as usize >= self.streams.len() { return Err(invalid("block stream number is out of range")); }
            decoded = decoded.checked_add(block.decoded_len).ok_or_else(|| invalid("decoded offset overflow"))?;
            if number > 0 && self.blocks[number - 1].compressed_start_bit >= block.compressed_start_bit {
                return Err(invalid("block compressed offsets are not increasing"));
            }
        }
        if decoded != self.decoded_len { return Err(invalid("decoded size does not match block records")); }
        let mut first_block = 0_u64;
        let mut stream_decoded = 0_u64;
        for (number, stream) in self.streams.iter().enumerate() {
            if !(1..=9).contains(&stream.block_size_100k) || stream.first_block != first_block || stream.decoded_start != stream_decoded
                || stream.eos_bit > source_bits { return Err(invalid("stream record is inconsistent")); }
            let end = stream.first_block.checked_add(stream.block_count).ok_or_else(|| invalid("stream block count overflow"))?;
            if end as usize > self.blocks.len() { return Err(invalid("stream block range is out of bounds")); }
            for block in &self.blocks[stream.first_block as usize..end as usize] {
                if block.stream != number as u64 { return Err(invalid("block belongs to the wrong stream")); }
            }
            first_block = end;
            stream_decoded = stream_decoded.checked_add(stream.decoded_len).ok_or_else(|| invalid("stream size overflow"))?;
        }
        if first_block as usize != self.blocks.len() || stream_decoded != self.decoded_len { return Err(invalid("stream records do not cover decoded data")); }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> Error { Error::InvalidIndex(message.into()) }

fn put_u32(out: &mut Vec<u8>, value: u32) { out.extend_from_slice(&value.to_le_bytes()); }

fn put_u64(out: &mut Vec<u8>, value: u64) { out.extend_from_slice(&value.to_le_bytes()); }

struct IndexReader<'a> { data: &'a [u8], pos: usize }

impl<'a> IndexReader<'a> {
    fn new(data: &'a [u8]) -> Self { Self { data, pos: 0 } }

    fn remaining(&self) -> usize { self.data.len() - self.pos }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(count).ok_or_else(|| invalid("offset overflow"))?;
        let value = self.data.get(self.pos..end).ok_or_else(|| invalid("truncated payload"))?;
        self.pos = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8> { Ok(self.take(1)?[0]) }

    fn u32(&mut self) -> Result<u32> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }

    fn u64(&mut self) -> Result<u64> { Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap())) }

    fn usize(&mut self, name: &str) -> Result<usize> { usize::try_from(self.u64()?).map_err(|_| invalid(format!("{name} does not fit this platform"))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_index_round_trip_and_source_binding() {
        let source = b"BZh9";
        let index = Index::new(source, 0, Vec::new(), Vec::new());
        let encoded = index.to_bytes();
        assert_eq!(Index::from_bytes(&encoded, source).unwrap(), index);
        assert!(matches!(Index::from_bytes(&encoded, b"BZh8"), Err(Error::InvalidIndex(_))));
    }

    #[test]
    fn rejects_tampered_payload() {
        let mut encoded = Index::new(b"BZh9", 0, Vec::new(), Vec::new()).to_bytes();
        encoded[0] ^= 1;
        assert!(matches!(Index::from_bytes(&encoded, b"BZh9"), Err(Error::InvalidIndex(_))));
    }
}
