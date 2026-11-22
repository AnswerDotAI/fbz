use io::ErrorKind;
use std::{collections::{HashMap, VecDeque}, io::{self, Read, Seek, SeekFrom}, path::Path};

use crate::{DecodeOptions, Error, Index, Result, Source, build_index, decode_block};

pub const DEFAULT_CACHE_LIMIT: usize = 64 * 1024 * 1024;

pub struct IndexedReader { source: Source, index: Index, position: u64, cache: BlockCache }

impl IndexedReader {
    pub fn open(path: impl AsRef<Path>, options: DecodeOptions) -> Result<Self> { Self::from_source(Source::open(path)?, None, options, DEFAULT_CACHE_LIMIT) }

    pub fn open_with_index(path: impl AsRef<Path>, index_path: impl AsRef<Path>, cache_limit: usize) -> Result<Self> {
        let source = Source::open(path)?;
        let index = Index::load(index_path, source.as_slice())?;
        Self::from_source(source, Some(index), DecodeOptions::default(), cache_limit)
    }

    pub fn from_bytes(data: Vec<u8>, options: DecodeOptions) -> Result<Self> { Self::from_source(Source::from_bytes(data), None, options, DEFAULT_CACHE_LIMIT) }

    pub fn from_bytes_with_index(data: Vec<u8>, encoded_index: &[u8], cache_limit: usize) -> Result<Self> {
        let source = Source::from_bytes(data);
        let index = Index::from_bytes(encoded_index, source.as_slice())?;
        Self::from_source(source, Some(index), DecodeOptions::default(), cache_limit)
    }

    pub fn from_source(source: Source, index: Option<Index>, options: DecodeOptions, cache_limit: usize) -> Result<Self> {
        let index = match index { Some(index) => index, None => build_index(source.as_slice(), options)? };
        Ok(Self { source, index, position: 0, cache: BlockCache::new(cache_limit) })
    }

    pub fn index(&self) -> &Index { &self.index }

    pub fn size(&self) -> u64 { self.index.decoded_len }

    pub fn position(&self) -> u64 { self.position }

    pub fn save_index(&self, path: impl AsRef<Path>) -> Result<()> { self.index.save(path) }

    fn block_number(&self, position: u64) -> Option<usize> {
        let number = self.index.blocks.partition_point(|block| block.decoded_start + block.decoded_len <= position);
        (number < self.index.blocks.len()).then_some(number)
    }

    fn read_block_part(&mut self, number: usize, output: &mut [u8]) -> Result<usize> {
        let block = &self.index.blocks[number];
        let offset = usize::try_from(self.position - block.decoded_start)
            .map_err(|_| Error::InvalidConfiguration("decoded block offset does not fit this platform".into()))?;
        if let Some(cached) = self.cache.get(number) {
            let count = output.len().min(cached.len() - offset);
            output[..count].copy_from_slice(&cached[offset..offset + count]);
            return Ok(count);
        }
        let stream = &self.index.streams[block.stream as usize];
        let decoded = decode_block(self.source.as_slice(), block.compressed_start_bit, block.compressed_end_bit, stream.block_size_100k, block.expected_crc)?;
        if decoded.len() as u64 != block.decoded_len { return Err(Error::InvalidIndex("decoded block length does not match index".into())); }
        let count = output.len().min(decoded.len() - offset);
        output[..count].copy_from_slice(&decoded[offset..offset + count]);
        self.cache.insert(number, decoded);
        Ok(count)
    }
}

impl Read for IndexedReader {
    fn read(&mut self, mut output: &mut [u8]) -> io::Result<usize> {
        let requested = output.len();
        while !output.is_empty() && self.position < self.index.decoded_len {
            let number = self.block_number(self.position).ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "index has a decoded gap"))?;
            let count = self.read_block_part(number, output).map_err(io::Error::other)?;
            if count == 0 { return Err(io::Error::new(ErrorKind::InvalidData, "decoder made no progress")); }
            self.position += count as u64;
            output = &mut output[count..];
        }
        Ok(requested - output.len())
    }
}

impl Seek for IndexedReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(position) => i128::from(position), SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.index.decoded_len) + i128::from(offset)
        };
        if !(0..=i128::from(self.index.decoded_len)).contains(&next) { return Err(io::Error::new(ErrorKind::InvalidInput, "seek outside decompressed data")); }
        self.position = next as u64;
        Ok(self.position)
    }
}

struct BlockCache { entries: HashMap<usize, Vec<u8>>, order: VecDeque<usize>, bytes: usize, limit: usize }

impl BlockCache {
    fn new(limit: usize) -> Self { Self { entries: HashMap::new(), order: VecDeque::new(), bytes: 0, limit } }

    fn get(&mut self, number: usize) -> Option<&Vec<u8>> {
        if !self.entries.contains_key(&number) { return None; }
        self.order.retain(|&entry| entry != number);
        self.order.push_back(number);
        self.entries.get(&number)
    }

    fn insert(&mut self, number: usize, data: Vec<u8>) {
        if data.len() > self.limit { return; }
        while self.bytes + data.len() > self.limit {
            let Some(oldest) = self.order.pop_front() else { break };
            if let Some(removed) = self.entries.remove(&oldest) { self.bytes -= removed.len(); }
        }
        self.bytes += data.len();
        self.order.push_back(number);
        self.entries.insert(number, data);
    }
}
