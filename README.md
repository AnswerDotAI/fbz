# fbz

**Fast, reliable parallel compression and decompression.**

`fbz` is one CLI for compressing and decompressing bzip2, gzip, LZ4, ZIP, and compressed tar archives. It selects formats from filenames or magic, uses the available CPU cores, validates every decoded stream, and safely creates and extracts archives. The stream codecs are also available through Rust and Python APIs.

`fbz` was created because we found existing tools tended to be too slow (as the benchmarks below show) or too unreliable (e.g `pbzip2` 1.1.13 fails to decompress the full English Wikipedia archive). And we wanted a single tool we could use for all common formats with a single CLI interface.

## Performance

The same 80.5 MiB SimpleWiki XML payload is used in every row with automatic thread selection. Each CLI is warmed once, then measured once on the primary Apple Silicon development machine.

### Decompression

Stream formats are fully decoded and validated without writing output; archives are extracted.

| Format | `fbz` | Familiar tool | Speedup |
|---|---:|---:|---:|
| `.bz2` | 163 ms | `bzip2`: 1.17 s | 7.2x |
| `.tar.bz2` | 152 ms | `tar`: 1.17 s | 7.7x |
| `.zip` (18 files) | 30 ms | `unzip`: 360 ms | 12x |
| `.gz` | 31 ms | `gzip`: 76 ms | 2.5x |
| `.tar.gz` | 57 ms | `tar`: 118 ms | 2.1x |
| `.lz4` | 55 ms | `lz4`: 56 ms | 1.0x |

### Compression

The standalone rows write compressed bytes to a sink. Archive rows create a real archive; the ZIP rows also show that the scheduler handles one large file and many ordinary files without nested parallelism.

| Format | `fbz` | Familiar tool | Relative speed |
|---|---:|---:|---:|
| `.bz2` | 736 ms | `bzip2`: 3.30 s | 4.5x |
| `.tar.bz2` | 729 ms | `tar`: 3.34 s | 4.6x |
| `.gz` | 147 ms | `gzip`: 1.40 s | 9.5x |
| `.tar.gz` | 144 ms | `tar`: 1.44 s | 10x |
| `.zip` (1 file) | 142 ms | `zip`: 1.47 s | 10x |
| `.zip` (18 files) | 138 ms | `zip`: 1.47 s | 11x |
| `.lz4` | 35 ms | `lz4`: 29 ms | 0.83x |

The fbz LZ4 output was about 7% smaller than the reference output on this payload. Other compressed sizes were within 1% of their familiar tools.

See [Benchmarking details](#benchmarking-details) for the details.

## Install

PyPI wheels contain both the Python module and the native `fbz` executable—there is no Python CLI wrapper:

```bash
pip install fbz
```

Install the native CLI with Homebrew:

```bash
brew tap AnswerDotAI/tap
brew trust --tap AnswerDotAI/tap
brew install fbz
```

Python 3.10 and later are supported. Prebuilt wheels target Linux on x86-64 and ARM64, and macOS on ARM64. macOS Intel is best-effort and can build from source.

Install the native CLI or add the Rust library from crates.io:

```bash
cargo install fbz
cargo add fbz
```

## CLI

| Command | Alias | Operation |
|---|---|---|
| `compress` | `c` | Compress files or create tar/ZIP archives |
| `decompress` | `d` | Decompress files or extract archives |
| `list` | `l` | Validate and list codec structure or ZIP entries |
| `test` | | Validate without writing output |
| `index` | | Build bzip2 seek indexes |

A path without a command defaults to decompression: `fbz file.gz` is equivalent to `fbz d file.gz`. The first positional argument selects a command only if it exactly matches a command name or alias; option values do not count. For a file named `c` or `test`, use `./c` or an explicit command such as `fbz d test`. Command recognition never depends on which files exist.

Use `fbz --help` for the command overview and `fbz c --help` for compression and filtering options. Threads (`-P`), memory limit, and quiet (`-q`) are shared options and can appear before or after the command. Other options belong after their command, or accompany a bare decompression path.

### Decompression

`.bz2`, `.bzip2`, `.gz`, `.gzip`, and `.lz4` select their corresponding decoder and are removed from the output name. Compressed tar names—`.tar.bz2`, `.tar.bzip2`, `.tbz`, `.tbz2`, `.tar.gz`, `.tar.gzip`, `.tgz`, and `.tar.lz4`—and `.zip` automatically extract into the current directory or `-C/--output-dir`. `-x/--extract` forces archive extraction for stdin or an unusual filename; an explicit `-o/--output` instead writes a decoded tar stream, but is invalid for ZIP because ZIP has no single decoded byte stream. For stdin and unrecognised extensions, bzip2, gzip, LZ4, or ZIP magic selects the format. Other non-archive input names gain `.out`.

```bash
fbz dump.xml.bz2                   # write dump.xml
fbz events.json.gz                 # write events.json
fbz events.json.lz4                # write events.json
fbz source.tar.gz                  # extract into the current directory
fbz source.tar.lz4 -C unpacked     # stream-decode and extract
fbz source.tbz2 -C unpacked        # extract into unpacked/
fbz dataset.zip -C unpacked        # extract ZIP entries adaptively in parallel
fbz d --extract -C unpacked -      # extract tar or ZIP data from stdin
fbz source.tgz -o source.tar       # decode without extracting
fbz dump.xml.bz2 -o result.xml     # choose the decoded output path
fbz dump.xml.bz2 -o -              # write decoded bytes to stdout
```

### Compression

`compress` (or `c`) compresses files. An output suffix selects the format; when `-o` is omitted, `--format` selects it and fbz appends the conventional suffix. Standalone streams accept stdin and can write stdout. Tar and ZIP creation accept multiple filesystem inputs and stream output without an intermediate tar or plaintext file.

```bash
fbz c --format bzip2 dump.xml       # write dump.xml.bz2
fbz c events.json -o events.json.gz # infer gzip from the output
fbz c data -o data.lz4              # independent-block LZ4 frame
fbz c src docs -o source.tar.gz     # stream tar directly into gzip
fbz c src docs -o source.tar.bz2    # stream tar directly into bzip2
fbz c src docs -o source.zip        # adaptive parallel ZIP creation
fbz c --format gzip -o - - < events # write one gzip member to stdout
```

### Selecting archive contents

Tar and ZIP creation include hidden and ignored files by default. Add `-i/--ignore` to honour `.gitignore`, `.ignore`, `.rgignore`, and Git's other ignore rules without hiding dotfiles. Exclude Git metadata explicitly with `-E .git`.

```bash
fbz c -i project -o source.zip -E .git
fbz c project -o code.tar.gz -e py -e ipynb -E tests
fbz c project -o docs.zip --include 'docs/**' --include README.md
fbz c -ni project -o source.zip -E .git
```

`-E/--exclude GLOB`, `--include GLOB`, and `-e/--extension EXT` are repeatable. Repeated includes or extensions match any supplied value; combining includes and extensions requires both to match. Excludes always win, independent of argument order. Includes do not override ignore rules.

Globs without `/` match names at any depth. Globs containing `/` match paths relative to each input directory. `*` matches within a path component; `**` spans directories. Excluding a directory prunes its entire subtree, while includes never block traversal to matching descendants. Matching is case-sensitive. Quote globs to prevent shell expansion.

Symlinks are stored as links, including dangling and explicitly named links. Selected entries retain their required parent directories. With no positive filters, empty directories are kept. Special entries such as sockets, FIFOs, and devices are rejected rather than silently omitted. The output archive itself is excluded if it is inside an input directory. No matches is an error rather than an empty archive.

`-n/--dry-run` prints the selected archive entry names to stdout and an entry-count summary to stderr, without creating or changing the archive or output directory. `--quiet` suppresses the summary. These five options apply only to tar/ZIP creation; other modes reject them.

### Multiple inputs and inspection

Multiple inputs are processed in order, with parallelism applied inside each compressed stream. `-C/--output-dir` collects decoded files and is the extraction root for archives:

```bash
fbz data/*.bz2 logs/*.gz -C decoded
fbz data/*.bz2 logs/*.gz -C decoded --skip-existing
fbz backups/*.tgz -C restored
fbz datasets/*.zip -C restored
```

Validation and inspection use their own commands:

```bash
fbz test dump.xml.bz2          # fully decode and validate, writing nothing
fbz index dump.xml.bz2         # write dump.xml.bz2.fbz2i (bzip2 only)
fbz l events.json.gz           # print the validated member/block layout
fbz l events.json.lz4          # print the validated frame/block layout
fbz l dataset.zip             # print the validated entry layout
fbz l --json dump.xml.bz2      # emit the complete layout as JSON
```

Human-readable `list` output labels each input when given multiple files; JSON output is one object for one input and an array for multiple inputs. The old `-z/--compress`, `--list`, `--test`, and `--index` flags are no longer accepted.

## Python

The Python API exposes compression, decompression, validation, and streaming reads for all three stream formats. Bzip2 additionally supports indexed seeking and structural scanning.

### One-shot compression

```python
import fbz

compressed = fbz.compress(plain_bytes, "gzip", level=6)
```

The format is `"bzip2"`, `"gzip"`, or `"lz4"`. `threads=0` selects automatically, `memory_limit` bounds scheduled work, and the format-specific default level is used when `level` is omitted.

### One-shot decompression and validation

```python
import fbz

plain = fbz.decompress(compressed_bytes)
fbz.test("dump.xml.bz2")  # returns None after successful validation
```

`decompress` accepts a bytes-like object, detects bzip2, gzip, or LZ4 from its magic, and returns `bytes`. Pass `format="gzip"` (or `"bzip2"`/`"lz4"`) to override detection. `test` accepts either compressed bytes or a path and avoids retaining the decoded result.

### Streaming reads

`fbz.open` and `fbz.Reader` start decoding a file immediately and implement Python's raw binary I/O interface. They do not index first or retain the plaintext; wrap them in `io.BufferedReader` when buffering is useful:

```python
import io, fbz

with io.BufferedReader(fbz.open("events.json.gz")) as f:
    header = f.read(4096)
```

They validate the complete stream and report a late checksum failure as `fbz.BadCompressedFile` rather than EOF. Dropping or closing a reader early cancels and joins its workers. Compressed tar inputs yield tar bytes; ZIP remains an archive rather than a byte-stream reader.

### Seekable reads and persistent indexes

`fbz.open_indexed` returns a seekable bzip2 `io.RawIOBase`. Opening without an index performs a complete validation pass and builds an in-memory block index; `build_index` can persist that work for later processes:

```python
import fbz

fbz.build_index("dump.xml.bz2", "dump.xml.bz2.fbz2i")

with fbz.open_indexed("dump.xml.bz2", index="dump.xml.bz2.fbz2i") as f:
    f.seek(1_000_000_000)
    chunk = f.read(64 * 1024)
    print(f.tell(), f.size)
```

Building an index fully decodes into a sink but does not write or retain the plaintext. Indexes contain compressed and decoded block offsets and are bound to the exact compressed source by its length and BLAKE3 hash. Loading one verifies that identity without decoding the whole payload; subsequent reads decode only the blocks needed for the requested range and cache recent blocks. `cache_limit` controls that cache. Path sources are memory-mapped, while bytes-like sources stay in memory.

### Structural scanning

`scan` cheaply finds candidate stream headers and bit-level block markers without decoding:

```python
import bz2
from fbz import scan

result = scan(bz2.compress(b"hello"))
assert result.blocks[0].bit_offset == 32
```

Scan results are deliberately untrusted candidates. Use `test`, `decompress`, `build_index`, or successfully exhaust a streaming reader when validation is required.

## Rust

The streaming API accepts any `Write` destination and uses the serial fast path when `threads` is one:

```rust
use fbz::{DecodeOptions, Source, decompress_to_writer};

fn main() -> fbz::Result<()> {
    let source = Source::open("dump.xml.bz2")?;
    let mut output = std::io::stdout().lock();
    decompress_to_writer(source.as_slice(), &mut output, DecodeOptions::default())?;
    Ok(())
}
```

`decompress` returns a `Vec<u8>` and detects bzip2, gzip, or LZ4 from magic. Set `DecodeOptions::format` to a `DecodeFormat` variant to override detection. `decompress_to_writer` and its progress variant use the same dispatcher and codec implementations. The bzip2-specific `decode_to_writer` returns a validated `Index`, while `build_index` validates into a sink. `IndexedReader` implements `Read` and `Seek`; it can build an index itself or load a persisted one with `open_with_index`.

The unified compression API covers bzip2, gzip, and LZ4 and writes incrementally:

```rust
use fbz::{EncodeFormat, EncodeOptions, compress_to_writer};

fn main() -> fbz::Result<()> {
    let mut input = std::fs::File::open("events.json")?;
    let mut output = std::fs::File::create("events.json.gz")?;
    compress_to_writer(&mut input, &mut output, EncodeFormat::Gzip, EncodeOptions::default())?;
    Ok(())
}
```

`compress` returns a `Vec<u8>`, while `Encoder<W>` implements `Write` for producers that generate data incrementally. `EncodeOptions` controls worker count, memory budget, and compression level. Gzip produces one standard member, LZ4 produces a standard independent-block frame, and bzip2 produces an ordinary `BZh1`–`BZh9` stream; none requires an fbz decoder.

`zip::create_to_writer` creates stored/DEFLATE ZIP and Zip64 archives from an exact list of `zip::PathInput` entries. Directory entries store only the directory itself; callers must supply its selected descendants explicitly. For tar composition, feed a `gzip::Encoder`, `lz4::Encoder`, or `Bzip2Encoder` directly to a streaming `tar::Builder`, which is the same composition used by the CLI.

The in-repo gzip decoder is available separately so callers can choose explicitly:

```rust
let plain = fbz::gzip::decompress(&compressed_gzip)?;
```

`gzip::decompress_to_writer` and `gzip::decompress_to_writer_with_options` return a validated report containing gzip member metadata, each DEFLATE block's kind and ranges, and counts of accepted speculative and serial-fallback chunks. They support stored, fixed-Huffman, and dynamic-Huffman blocks, optional gzip headers, and concatenated members.

The raw shared codec is available as `fbz::deflate::decompress_to_sink_with_options_and_progress`; gzip framing and ZIP extraction both use this exact decoder.

The in-repo LZ4 frame decoder has the same one-shot, writer, options, progress, and report shapes as gzip:

```rust
let plain = fbz::lz4::decompress(&compressed_lz4)?;
```

It accepts standard independent or linked blocks, stored blocks, all four standard block maxima, optional block/content checksums and sizes, concatenated frames, and skippable frames. External dictionaries and the obsolete legacy frame format are intentionally unsupported.

### Streaming reads

`fbz::Reader` provides a normal `std::io::Read` over bzip2, gzip, or LZ4 files without a preliminary indexing or validation pass:

```rust
use std::io::{BufReader, Read};
use fbz::{DecodeOptions, Reader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let reader = Reader::open("dump.xml.bz2", DecodeOptions::default())?;
    let mut reader = BufReader::new(reader);
    let mut header = [0; 4096];
    reader.read_exact(&mut header)?;
    Ok(())
}
```

Magic takes priority over the filename extension, with the extension used as a fallback for damaged headers; an explicit `DecodeOptions::format` overrides both. The decoder runs on an owned worker thread and transfers completed decoder allocations through a zero-capacity pipe; it neither materializes the plaintext nor writes an intermediate file. `DecodeOptions` controls format selection, decoder threads, and speculative memory. Dropping early disconnects the pipe, cancels outstanding work, and joins the worker.

Checksum errors discovered after output has begun are returned by a later `read()` call. Therefore only successful EOF establishes that the complete stream was valid; dropping early deliberately does not finish validation. Compressed tar inputs yield the decoded tar byte stream rather than extracting it. ZIP is not exposed through `Reader` because an archive has no single decoded byte stream.

## CLI output safety

- Existing generated files and archive entries are rejected by default. `--force` replaces them; `--skip-existing` applies to standalone outputs and newly created archives rather than extraction into an existing tree.
- Decoded-file outputs use a same-directory temporary file and become visible atomically only after successful checksum validation.
- Tar entries stream into a same-filesystem staging directory through a bounded pipe. ZIP entries decode directly into the same staging scheme. Entries are preflighted and moved into the destination only after every relevant compression stream and archive structure validates, so a late CRC failure leaves no extracted files.
- Tar and ZIP paths and link targets are confined to the destination. ZIP rejects unsafe or duplicate paths; tar safely skips unsafe entries. New entries use the archive's permissions and modification times where provided. Standalone decoded files inherit those values from the compressed input.
- `--rm` removes an input only after its compressed or decoded output has been committed successfully. Archive creation deliberately does not remove its source tree.
- `--max-output SIZE` limits decoded bytes per input, including tar framing and padding. Sizes accept binary suffixes such as `K`, `MiB`, and `G`.

Long interactive standalone operations report completion, throughput, compression ratio, and ETA on stderr. Progress is disabled automatically when stderr is redirected; `-q/--quiet` also suppresses progress and skip notices.

`-P/--threads 0`, the default, selects parallelism automatically; an explicit positive value is honoured by every codec. `--memory-limit` is the byte budget for in-flight scheduler reservations and defaults to `1G`; it bounds queued input, working state, and retained results rather than promising an exact process-RSS ceiling. Automatic bzip2 compression stops at 12 workers because its BWT working sets reach a clear throughput plateau there. Automatic LZ4 decoding stops at four workers because it is memory-bandwidth bound; explicit `-P` values remain unchanged. Gzip and standalone LZ4 compression divide one standard stream into independently encoded ordered segments or blocks. ZIP uses one level of parallelism at a time: large entries use the parallel DEFLATE engine, while archives of ordinary entries process files concurrently without nested worker pools.

## Benchmarking details

The headline benchmarks use the first 84,423,012 decoded bytes of SimpleWiki for every format. In the decompression table, standalone bzip2 and gzip use each tool's validation mode; ZIP extracts 18 equal files, while compressed tar extracts one. In the compression table, standalone codecs write to a sink, tar wraps one file, and ZIP creates either one large entry or 18 equal entries. Each comparison performs the same work on both sides. All results are single local release-mode observations after one untimed warm-up, not statistical aggregates.

The familiar reference CLIs are the system `bzip2`, `gzip`, and `tar`; Apple Info-ZIP `zip`/`unzip` 3.0/6.00; and Homebrew `lz4` 1.10.0. The detailed fixture-generation and single-run commands live in [DEV.md](DEV.md), alongside in-process codec comparisons and separate memory diagnostics. RSS is deliberately omitted from the headline tables: it is bounded and configurable, but the parallel encoders trade memory for throughput and the exact figures are implementation diagnostics rather than user-visible work.

On the complete 1.57 GiB SimpleWiki XML recompressed with system `gzip -6`, `fbz` validated the stream in 0.33 seconds, compared with 0.36 seconds for a local `rapidgzip-rust` checkout and 1.37 seconds for Apple `gzip`. This larger result is kept here because it exercises sustained parallel gzip decoding; it is not mixed into the common-payload headline table.

### Reliability on large inputs

Homebrew `pbzip2` 1.1.13 could not safely decompress the complete 26,668,484,995-byte English Wikipedia multistream dump on this machine. It segfaulted, and repeated attempts produced divergent and truncated plaintext. Successful smaller-file benchmark results therefore do not establish full-file reliability, which is why `fbz` treats structural and checksum validation as part of decompression.

## Implementation and compatibility

The production codec logic is portable Rust. The bzip2 decoder uses a tuned 4096-entry Huffman lookup table for codes up to 12 bits and canonical fallback for longer codes. A structural scan finds possible non-byte-aligned block markers; these remain speculative until ordered decoding establishes the exact stream chain and validates all block and combined-stream CRCs. A rolling scheduler keeps workers busy across concatenated streams while bounding decoded results awaiting validation.

The gzip backend implements RFC 1952 framing and DEFLATE directly in this repository. For sufficiently large dynamic-Huffman inputs its decoder discovers independently decodable boundaries, represents unknown predecessor bytes as compact markers, and resolves only the suffix needed for the next 32 KiB history window. Its encoder schedules 1 MiB raw-DEFLATE segments with the preceding 32 KiB dictionary, joins their byte-aligned boundaries in order, and writes one ordinary gzip member and trailer. The same raw-DEFLATE encoder creates ZIP entries. Fixed, dynamic, and stored blocks are selected by encoded size.

LZ4 framing and blocks are likewise implemented in safe Rust. Decoding schedules independent blocks in bounded batches and retains only 64 KiB for linked history. Compression emits independent blocks so they can be encoded and later decoded in parallel; compressible blocks use a fast latest-match table, with the shared hash-chain matcher available at higher levels, and incompressible blocks are stored. Header, block, and content XXH32 checksums are handled where present.

The bzip2 encoder splits ordinary `BZh1`–`BZh9` streams at their natural block boundaries. RLE1 runs are formed incrementally, BWT/MTF/RLE2/Huffman work runs independently per block, and exact bit strings plus combined CRCs are committed in order. The BWT uses a safe SA-IS suffix array. Decoder and encoder share the bzip2 CRC implementation.

ZIP extraction uses the mature `zip` crate with codec features disabled for container structure and metadata, then feeds raw entry ranges through fbz's decoder. ZIP creation writes the small amount of required structure directly so externally produced raw-DEFLATE segments can stream without being copied through a second codec. It supports stored and DEFLATE entries, Zip64, data descriptors, Unix symlinks/modes, and extended timestamps. Tar creation and extraction use the mature `tar` crate as a streaming structural layer. Encryption and uncommon legacy ZIP methods are intentionally unsupported. `crc32fast` and `twox-hash` are the production checksum helpers; `libbz2-rs-sys`, `flate2`, and `lz4_flex` are dev-only differential oracles.

Legacy randomized blocks generated by bzip2 releases before 0.9.5 are intentionally unsupported. Normal `BZh1` through `BZh9` streams and concatenated streams are supported.

## Research lineage and credits

The gzip work builds on Maximilian Knespel and Holger Brunst's HPDC '23 paper, [*Rapidgzip: Parallel Decompression and Seeking in Gzip Files Using Cache Prefetching*](https://doi.org/10.1145/3588195.3592992). In particular, fbz adapts its central idea of starting DEFLATE decoding without the preceding 32 KiB window, representing uncertain output until the true history becomes available, and committing independently decoded chunks in order.

The open-source implementations and codebases consulted were:

- [`rapidgzip`](https://github.com/mxmlnkn/rapidgzip), the C++ implementation described by the paper.
- [`rapidgzip-rust`](https://github.com/COMBINE-lab/rapidgzip-rust), a pure-Rust reimplementation and fbz's local gzip performance and memory reference.
- [`librapidarchive`](https://github.com/mxmlnkn/librapidarchive), an experimental shared architecture for parallel bzip2 and gzip access.
- [`indexed_bzip2`](https://github.com/mxmlnkn/indexed_bzip2), for non-byte-aligned marker scanning, independent bzip2 block decoding, ordered prefetch, and indexed seeking.
- [`zip`](https://github.com/zip-rs/zip2), used without codec features for maintained ZIP structure and metadata handling.
- [`LZ4`](https://github.com/lz4/lz4), the reference format and Homebrew CLI performance baseline.
- [`lz4_flex`](https://github.com/pseitz/lz4_flex), used dev-only to generate a broad interoperability matrix and benchmark frames.
- [`lz4-rs`](https://github.com/bozaro/lz4-rs), consulted as a second local implementation reference.
- [`crabz2`](https://github.com/jwmurray/crabz2), whose MIT-licensed BWT, MTF/RLE2, and grouped-Huffman encoder machinery was adapted for fbz's block-parallel bzip2 compressor.
- Rob Landley's 0BSD [`bzcat` implementation in Toybox](https://github.com/landley/toybox), from which fbz's specialised bzip2 decoder is derived.

## Development

[DEV.md](DEV.md) documents the architecture, test strategy, benchmark fixture generation, build commands, and release process.

`fbz` is licensed under the [Apache License 2.0](LICENSE). The adapted crabz2 encoder files retain their bundled MIT license and attribution.
