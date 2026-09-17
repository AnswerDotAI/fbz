# Development

`fbz` is a mixed Rust/PyO3 project. The Rust crate is the implementation and public Rust API; `python/fbz/` is the public Python package over the private `fbz._core` extension.

## Architecture

The CLI has typed `compress/c`, `decompress/d`, `list/l`, `test`, and `index` commands. Only threads, memory limit, and quiet are global; each command owns its remaining options. Before parsing, `cli_args` checks the first positional against Clap's command names and aliases, inserting `decompress` for a bare path. It uses the same Clap schema to skip option values, including attached and combined short options, and never consults the filesystem. The old mode flags are not supported.

```text
src/bitreader.rs      bounded MSB-first in-memory bit reads
src/block.rs          independently decodable block construction and validation
src/crc.rs            incremental bzip2 block and combined-stream CRC primitives
src/decode.rs         serial/parallel decode scheduling and index construction
src/decoder.rs        bzip2 block machinery and 12-bit Huffman fast tables
src/bz2_encode/       block-parallel BWT/MTF/Huffman bzip2 encoder
src/encode.rs         unified stream compression API and progress adapter
src/format.rs         cheap structural scan for header and marker candidates
src/gzip.rs           gzip framing, LSB-first DEFLATE implementation, CRC32, and reports
src/deflate.rs        format-neutral raw-DEFLATE API shared by gzip and ZIP
src/deflate_encode.rs segmented raw-DEFLATE and gzip encoder machinery
src/lz4.rs            safe LZ4 frame/block decoder and independent-block scheduling
src/lz4_encode.rs     independent-block LZ4 frame encoder
src/matchfinder.rs    shared latest-match and hash-chain LZ match finders
src/history.rs        shared overlapping LZ back-reference expansion
src/output.rs         owned/borrowed decoded-output sink abstraction
src/pipeline.rs       shared ordered, byte-budgeted, staged worker scheduler
src/index.rs           stable persistent index format
src/indexed.rs         seekable decoded view and block cache
src/lib.rs            public Rust API and private PyO3 binding
src/source.rs         owned and memory-mapped compressed sources
src/zip.rs            streaming ZIP/Zip64 creation over raw DEFLATE
src/bin/fbz/archive_create.rs shared rgapi selection, safe archive naming, and dry-run
src/bin/fbz/archive_extract.rs shared same-filesystem staging and atomic commit
src/bin/fbz/tar_create.rs  streaming tar composition over each encoder
src/bin/fbz/tar_extract.rs  bounded decode-to-tar bridge
src/bin/fbz/zip_extract.rs  ZIP parsing policy and adaptive entry extraction
python/fbz/       thin Python I/O wrapper over fbz._core
build_backend.py      stage the native CLI for PEP 517 wheel builds
tests/corpus/         selected upstream conformance and corruption fixtures
tests/                Rust CLI/corpus and Python API integration tests
tools/stage_binaries.py copy the release executable into Maturin wheel data
```

The current bzip2 scanner deliberately does not treat 48-bit marker matches or later `BZh` headers as validated structure. Full decoding must establish the exact block chain and validate every block CRC plus the combined stream CRC before marker candidates can become trusted index entries. Python integration tests use standard-library `bz2`/libbz2 as an independent fixture generator.

The gzip decoder is an in-repo RFC 1952/RFC 1951 implementation rather than a wrapper around a production codec. It parses optional headers and concatenated members, decodes stored/fixed/dynamic blocks, maintains the 32 KiB LZ77 history, and validates FHCRC, CRC32, and ISIZE. For large inputs, independently discovered dynamic-block boundaries seed unknown history with compact markers. Primary jobs compute the CRC of each known clean suffix before ordered resolution. Resolution workers resolve the marker prefix, hash that prefix, and combine the two CRCs without rescanning the clean bytes. A marker-free history switches the same decoder to byte output. Reports retain member boundaries, DEFLATE block ranges, and accepted/fallback chunk counts. `crc32fast` is the sole production helper; `flate2` is dev-only.

The gzip encoder creates one interoperable member rather than concatenating independently compressed members. It carries each segment's preceding 32 KiB dictionary into a 1 MiB raw-DEFLATE job, emits a non-final byte-aligned sync boundary, then commits segments in order before one final block and CRC32/ISIZE trailer. Fixed, dynamic, and stored encodings are built for each segment and the shortest is retained. This exposes within-stream parallelism while preserving normal gzip semantics and compression ratio. The raw encoder is also the ZIP creation codec.

The bzip2 encoder incrementally forms RLE1 blocks and schedules BWT, MTF/RLE2, and grouped Huffman coding independently. Completed bit strings are concatenated exactly rather than padded to bytes, and block CRCs are combined in stream order. The safe SA-IS BWT, MTF/RLE2, and Huffman pieces are adapted from crabz2 0.4.0 under the bundled MIT license; fbz supplies the streaming orchestration, byte-budgeted scheduler, and shared CRC. Automatic mode caps at 12 BWT workers because 12→18 improved the SimpleWiki measurement only 7% while increasing RSS from about 445 to 581 MiB. Explicit `-P` remains exact.

The decoder remains independent of files, Python, and the CLI. Parallel scanning/decoding and indexed seeking are layered over it. Native workers never call Python. Large offsets use explicit 64-bit bit/byte types, and speculative block-marker hits are accepted only when they form an exact stream chain with valid block and combined stream CRCs.

Core decode APIs report completed compressed and decoded byte counts without knowing anything about terminals. The public `decompress` and writer variants detect bzip2, gzip, or LZ4 magic through one dispatcher; `DecodeOptions::format` provides an explicit override. Bzip2 indexing remains a separate API because its preliminary validation, seeking, and cache controls have different semantics. The CLI selects bzip2, gzip, LZ4, or ZIP by a recognised extension and falls back to magic for stdin or unknown names. It layers delayed, rate-limited TTY progress rendering over the shared callbacks; redirected stderr and `--quiet` produce no progress output. Decoded files use same-directory temporary files and atomic persistence, then inherit the compressed input's modification time and permissions. `--rm` removes an input only after decode, persistence, and metadata copying all succeed. An `OutputSink` wrapper enforces output-size limits, so each decoder has one code path for files, stdout, validation, listing, and archive extraction.

The LZ4 decoder parses current frames, concatenation, and skippable frames itself. It validates descriptor bits and XXH32 header, block, and content checksums, and bounds every literal and match before writing. A frame header creates an incremental block cursor rather than a complete layout. Independent blocks are gathered into at most 64-entry batches—only until there is enough work to amortize the pool—and become ordinary `pipeline::Job`s. A parse failure discovered during bounded look-ahead is held until every earlier valid block has decoded and emitted, preserving stream error order. Compressed jobs reserve the frame's declared maximum decoded block size; stored blocks borrow their source bytes and reserve no decoded allocation. Frames containing only stored blocks without block checksums bypass the worker pool because their only remaining work is ordered output and optional content hashing. Retained results remain charged at their allocation capacity rather than logical length, so highly compressible blocks cannot understate memory use. If fewer than two natural blocks fit the speculative budget, decoding proceeds incrementally on the coordinator instead of rejecting the frame. The pool is created lazily and reused across concatenated frames. Linked frames use the same parser, block decoder, output sink, progress, and report path, but decode serially with a rolling 64 KiB history. This is one code path with a scheduling branch, not separate Reader and writer implementations.

The LZ4 encoder emits standard independent-block frames with a content checksum. Its block size adapts from 4 MiB down to 64 KiB when the memory budget is small. Levels 1–6 use a latest-position table and LZ4-style adaptive skip; levels 7–9 use the shared bounded hash chain. Word-at-a-time common-prefix comparison is shared by both matchers. Each job chooses compressed or stored representation, and the coordinator emits completed blocks in order without retaining a complete frame.

Tar format semantics use the mature `tar` crate, pinned from 0.4.46 and built without its optional xattr feature. Creation feeds `tar::Builder` directly into the selected streaming encoder, so the first tar bytes can be compressed immediately and there is no intermediate tar. Extraction uses a zero-capacity rendezvous channel to transfer each owned decoder chunk and its live suffix offset to `tar::Archive`; the channel queues no chunks and applies backpressure. Extraction writes immediately into a same-filesystem temporary directory, drains all trailing tar padding so codec validation completes, then preflights every destination conflict and moves entries into place with renames. Multiple archive inputs remain sequential so their per-codec worker pools cannot oversubscribe the global thread budget.

ZIP structure semantics use `zip` 8.6.0 with all codec features disabled. The crate parses the central directory, Zip64 fields, data descriptors, names, modes, symlink kinds, and timestamp extra fields; fbz reads each raw stored/DEFLATE range and sends it through its own validated codec path. Because the crate intentionally collapses duplicate raw names into its index map, a small bounds-checked central-record count detects and rejects that ambiguity before using its metadata. Parsing also rejects encryption, unsupported methods, escaping or equivalent paths, non-directory ancestors, overlapping data ranges, and ranges crossing the central directory. An aggregate declared-size check runs before extraction, and a per-entry sink prevents output exceeding its declaration before size and CRC32 are checked. ZIP and tar share the same staging/preflight/rename implementation.

ZIP scheduling deliberately uses one parallelism level at a time. A sole DEFLATE entry at least 16 MiB compressed, or an entry at least 64 MiB in a multi-entry archive, uses all requested workers inside the raw DEFLATE decoder. Remaining entries run serial inner decoders concurrently on one Rayon pool. This keeps thread ownership and memory behaviour obvious, avoids nested oversubscription, and lets many ordinary entries naturally absorb stragglers through work stealing.

ZIP creation follows the same policy over uncompressed sizes. One file of at least 16 MiB uses the segmented raw-DEFLATE engine. In multi-file archives, entries at least 64 MiB use that path sequentially, while ordinary entries are compressed concurrently with serial inner DEFLATE. A custom structural writer is smaller and more direct here than contorting the `zip` crate to accept externally segmented raw bitstreams; it writes descriptors, central records, Zip64 structures, Unix modes/symlinks, and extended timestamps while retaining only central-directory metadata. The `zip` crate remains the maintained parser for extraction.

Archive creation selects paths once through `rgapi::find_iter` before opening the output. `archive_create.rs` owns the CLI filters, reconstructs required parent entries, rejects duplicate archive names, and excludes the output path. Defaults include hidden and ignored files and preserve symlinks. Tar and ZIP writers receive the same exact `zip::PathInput` list and never recurse; tar explicitly disables symlink following. Dry-run displays this same selection without creating output files or directories. Glob exclusions prune subtrees in rgapi, with `*` confined to a path component and `**` spanning directories.

The shared `pipeline.rs` scheduler provides ordered results, byte-budgeted admission, cancellation, and a staged priority queue. Bzip2 uses the rolling candidate path: workers reserve the maximum possible decoded block size, then shrink that reservation to actual retained output until ordered validation consumes or rejects it. Gzip decoding uses the staged path for speculative DEFLATE and priority marker resolution. LZ4 decoding and multi-entry ZIP use ordinary ordered jobs. Compression's long-lived `StreamingOrdered` pool accepts work as bytes arrive, retains output in key order, catches worker panics, and cancels queued work when dropped. Gzip, LZ4, and bzip2 encoders all use it rather than maintaining codec-specific thread/channel machinery. Reservations conservatively cover owned input, temporary working state, and retained output until the coordinator consumes it.

The shared `OutputSink` boundary accepts owned decoder chunks. Direct writer APIs adapt those chunks to `Write::write_all` without a channel or allocation copy. Rust `Reader` and streaming tar extraction instead use the same zero-capacity owned-chunk pipe, so completed bzip2 blocks, resolved parallel-gzip segments, and decoded LZ4 blocks move into the consumer rather than being copied into an intermediate pipe buffer. Python's `Reader` is a thin `io.RawIOBase` wrapper over that Rust reader and exposes native `read`/`readinto` operations while releasing the GIL during reads. Its safe buffer-protocol bridge copies at most 1 MiB per `readinto` call rather than retaining an allocation as large as an arbitrary caller-provided buffer. The pipe adds at most the consumer's current chunk and the producer's next blocked chunk beyond the scheduler budget. Its receiver owns a cancellation flag; bzip2 scanning checks that flag between bounded waves, while LZ4 checks it before parsing each serial block or parallel batch.

The non-indexing parallel bzip2 path decodes and emits its first CRC-validated block before scanning the remainder of the compressed source. That candidate is then reused by normal ordered assembly rather than decoded twice. Index construction retains the full scan-first path because it produces no output. `Reader` sends decoder success or failure separately from the byte pipe and treats a missing terminal status as an error, so a worker panic or corrupt trailer cannot appear as EOF. Reader errors are sticky across subsequent calls, and drop disconnects the pipe before joining the decoder thread.

The bzip2 decoder is safe scalar Rust designed for LLVM auto-vectorisation. Huffman decoding uses a 4096-entry direct table for codes up to 12 bits and canonical fallback for longer codes. Gzip uses full canonical lookup tables packed into `u16`, a branch-free 64 KiB marker-resolution lookup for large chunks, and `crc32fast::Hasher::combine` so CRC scanning runs with the resolution workers rather than serially in the coordinator. Gzip byte/marker output and LZ4 share `history::extend_match`, whose doubling copies handle overlapping matches in logarithmically many operations; format-specific history validation remains at each call site. The only unsafe codec operation marks a just-initialized `Vec` result as initialized after writing every spare-capacity byte. Add architecture-specific SIMD or further cross-codec abstraction only after profiling; `libbz2-rs-sys`, `flate2`, and `lz4_flex` remain dev-only differential oracles.

## Commands

```bash
cargo test
cargo test --release
cargo check --all-features
cargo build --release --bins
cargo package
python tools/stage_binaries.py
uv pip install --reinstall --no-deps -e .
pytest -q
ship-rs-build
```

Run `cargo fmt --check` after Rust edits and `chkstyle` after Python edits once tests pass.

## Correctness and performance acceptance

User-facing comparisons between installable CLIs are recorded only in [README Performance](README.md#performance). This file documents fixture reproduction, regression gates, and implementation-oriented diagnostics.

The normal release test path decodes selected valid and corrupt cases from the maintained upstream `bzip2-testfiles` collection. Generated byte distributions add differential coverage. Valid bzip2 outputs are compared byte-for-byte with `libbz2-rs-sys`. Gzip and raw-DEFLATE tests cover stored, fixed-Huffman, and dynamic-Huffman blocks; optional headers and FHCRC; concatenated members; exact end-of-stream boundaries; truncation; and trailer corruption across varied inputs and compression levels generated by `flate2`. LZ4 tests use `lz4_flex` to generate a matrix spanning empty, repetitive, byte-distribution, and pseudorandom inputs; all four standard block sizes; independent and linked blocks; and every checksum/content-size combination. The production decoders do not depend on any oracle.

Encoder tests cover empty, repetitive, patterned, multiblock, and incremental-write inputs. fbz and an independent implementation both decode every generated stream: system bzip2 for bzip2, `flate2` for gzip, and `lz4_flex` for LZ4. ZIP output is validated and extracted with Info-ZIP, including the large-entry segmented-DEFLATE path; CLI tests cover `.tar.bz2`, `.tar.gz`, `.tar.lz4`, and ZIP creation/extraction composition. The unified Rust and Python APIs have separate integration tests.

The normal release path contains warmed end-to-end performance regression gates capped at 1.3 times each oracle, allowing for noise on shared runners. The gzip gates independently exercise a highly compressible LZ77-heavy shape and an incompressible literal-heavy shape against `flate2`; the bzip2 gate uses `libbz2-rs-sys`. Representative local acceptance remains 1.2 times the corresponding oracle. The ignored full-wiki gzip test applies that threshold to rapidgzip-rust. Keep the whole release test suite below five seconds on the primary development laptop; individual timed workloads should normally be about 0.1 seconds or less.

### Local standalone CLI comparisons

`tests/stream_cli_perf.rs` reproduces the standalone bzip2 and gzip rows in the README using the common 84,423,012-byte SimpleWiki prefix. Each test fully validates one format with `fbz` and its system CLI, warming each executable once before exactly one measured run:

```bash
cargo test --release --test stream_cli_perf bzip2_cli_comparison -- --ignored --exact --nocapture
cargo test --release --test stream_cli_perf gzip_cli_comparison -- --ignored --exact --nocapture
```

Regenerate both compressed inputs with the shared instructions in [Local Wikipedia benchmarks](#local-wikipedia-benchmarks). Run the tests separately so their parallel decoders do not compete for the same cores. User-facing results belong only in the README; this section records reproduction rather than a duplicate table.

### Local compression comparisons

`tests/compression_cli_perf.rs` decodes `meta/simplewiki-first-5pct.xml.bz2` before timing and writes the resulting 84,423,012-byte payload into its temporary fixture. Standalone codecs write to a sink; archive tests write temporary files and report their sizes. Fixture creation and one warm-up per CLI are outside the measured interval, followed by exactly one child-process measurement. Run tests separately so encoders do not compete for cores:

```bash
cargo test --release --test compression_cli_perf bzip2_compression_cli_comparison -- --ignored --exact --nocapture
cargo test --release --test compression_cli_perf gzip_compression_cli_comparison -- --ignored --exact --nocapture
cargo test --release --test compression_cli_perf lz4_compression_cli_comparison -- --ignored --exact --nocapture
cargo test --release --test compression_cli_perf tar_bzip2_compression_cli_comparison -- --ignored --exact --nocapture
cargo test --release --test compression_cli_perf tar_gzip_compression_cli_comparison -- --ignored --exact --nocapture
cargo test --release --test compression_cli_perf zip_single_compression_cli_comparison -- --ignored --exact --nocapture
cargo test --release --test compression_cli_perf zip_many_compression_cli_comparison -- --ignored --exact --nocapture
```

Regenerate the underlying payload using [Regenerating the fixtures](#regenerating-the-fixtures). Benchmark scheduling does not use hard-coded source or compressed lengths; the shared fixture loader asserts the independently established decoded acceptance length so truncation cannot look fast. The familiar CLI timing results belong only in README. The same single runs recorded peak RSS for implementation work: fbz used 436 MiB for bzip2, 161 MiB for gzip, 97 MiB for LZ4, 162 MiB for one-entry ZIP, and 332 MiB for 18-entry ZIP. Reference CLIs used 8 MiB or less except multithreaded `lz4` at 47 MiB. These are observed process peaks, not the configurable scheduler reservation limit.

The bzip2 one-run-per-count diagnostic explains the 12-worker automatic cap:

```bash
cargo test --release --test compression_cli_perf bzip2_compression_thread_sweep -- --ignored --exact --nocapture
```

| Workers | Compression | Peak RSS |
|---:|---:|---:|
| 1 | 4.80 s | 54 MiB |
| 2 | 2.54 s | 98 MiB |
| 4 | 1.49 s | 175 MiB |
| 6 | 1.20 s | 240 MiB |
| 8 | 959 ms | 338 MiB |
| 12 | 746 ms | 445 MiB |
| 18 | 695 ms | 581 MiB |

ZIP's across-entry scheduler can be checked independently at 8, 12, and 18 workers:

```bash
cargo test --release --test compression_cli_perf zip_compression_thread_sweep -- --ignored --exact --nocapture
```

A single local sweep measured 220 ms / 226 MiB at 12 workers and 150 ms / 336 MiB at 18. That clear throughput gain does not support an automatic ZIP worker cap; the shared memory budget still bounds scheduled entries, and explicit `-P` remains available when lower memory is preferable.

`tests/compression_perf.rs` contains dev-only in-process comparisons against `flate2` and `lz4_flex`, plus an LZ4 worker sweep. They are useful for isolating codec work from CLI startup and I/O but stay out of README because they are not comparisons users can run as tools.

### Local Reader benchmark

`tests/reader_perf.rs` compares the public `Read` adapter with the direct writer path on 84.4 MiB SimpleWiki inputs. Each row is one release-mode run, and both timings include opening the file. The Reader path necessarily copies into the caller's buffer but transfers decoder-owned chunks into its rendezvous pipe without another copy. The LZ4 test builds its frame before timing:

```bash
cargo test --release --test reader_perf reader_writer_comparison -- --ignored --exact --nocapture
cargo test --release --test reader_perf lz4_reader_writer_comparison -- --ignored --exact --nocapture
```

| Format | First byte | Reader | Direct writer | Reader/writer |
|---|---:|---:|---:|---:|
| bzip2 | 36.627 ms | 205.081 ms | 183.465 ms | 1.118x |
| gzip | 11.509 ms | 28.119 ms | 24.515 ms | 1.147x |
| LZ4 | 6.513 ms | 39.213 ms | 39.020 ms | 1.005x |

The bzip2 first-byte measurement includes opening, mapping, and decoding its first block, but not the subsequent whole-input parallel marker scan. Gzip parses only the current member header before beginning DEFLATE work. LZ4 parses the current frame header and a bounded independent-block batch; it never constructs a complete frame layout. Keep this diagnostic single-run.

### Local LZ4 benchmark reproduction and diagnostics

`tests/lz4_perf.rs` decodes `meta/simplewiki-first-5pct.xml.bz2` before timing, then uses dev-only `lz4_flex` to create a standard Max4MB independent-block LZ4 frame with a content checksum. Fixture construction and both warm-ups are outside the measured intervals. One test then measures exactly one validation run of each CLI and records child-process memory without task-inspection permissions:

```bash
cargo test --release --test lz4_perf lz4_cli_comparison -- --ignored --exact --nocapture
cargo test --release --test lz4_perf lz4_thread_sweep -- --ignored --exact --nocapture
```

Regenerate the underlying SimpleWiki fixture using the shared instructions in [Local Wikipedia benchmarks](#local-wikipedia-benchmarks); no `.lz4` fixture is stored. This section retains only implementation diagnostics.

The one-measurement-per-count scaling diagnostic explains the four-worker automatic limit:

| Workers | Validation | Peak RSS |
|---:|---:|---:|
| 1 | 109.727 ms | 46.5 MiB |
| 2 | 54.995 ms | 58.9 MiB |
| 4 | 51.367 ms | 71.0 MiB |
| 6 | 55.286 ms | 79.2 MiB |
| 8 | 52.010 ms | 87.2 MiB |
| 12 | 55.300 ms | 103.4 MiB |
| 18 | 51.915 ms | 124.2 MiB |

Four workers are at the front of the noisy plateau while using much less memory than 8–18. Automatic mode therefore uses at most four LZ4 workers; an explicit `-P N` still requests exactly `N`.

`lz4_shape_diagnostics` isolates the two simple structural experiments:

```bash
cargo test --release --test lz4_perf lz4_shape_diagnostics -- --ignored --exact --nocapture
```

Replacing LZ4's offset-sized copy loop with the shared exponential back-reference expander reduced a single 4 MiB long-match decode from 10.588 ms to 0.278 ms. On a 16 MiB stored-only frame, the old worker path took 1.425 ms; bypassing its no-work pool took 1.286 ms, essentially flat in speed but with no unnecessary threads. Both figures are single measured runs after one warm-up per path.

### Local archive extraction benchmarks

`tests/archive_perf.rs` measures the tar layer on the real `meta/simplewiki-first-5pct.xml.bz2` corpus. Fixture decoding and gzip/bzip2 recompression finish before timing. Each ignored test warms one target and measures it once. Run only the implementation changed:

```bash
cargo test --release --test archive_perf tgz_fbz_overhead -- --ignored --exact --nocapture
cargo test --release --test archive_perf tgz_system_reference -- --ignored --exact --nocapture
cargo test --release --test archive_perf tbz2_fbz_overhead -- --ignored --exact --nocapture
cargo test --release --test archive_perf tbz2_system_reference -- --ignored --exact --nocapture
cargo test --release --test archive_perf tar_crate_reference -- --ignored --exact --nocapture
cargo test --release --test archive_perf tgz_output_cadence -- --ignored --exact --nocapture
```

These are the internal overhead measurements after owned-suffix transfer and the 512 KiB gzip grid change:

| Format | Raw decode | Extraction/raw |
|---|---:|---:|
| `.tgz` | 39.919 ms | 1.424x |
| `.tar.bz2` | 148.411 ms | 1.023x |

Direct extraction of the uncompressed in-memory tar through the `tar` crate took 32.069 ms. Raw gzip decode plus direct tar extraction totals 71.988 ms. The combined pipeline hides 15.138 ms, or 47%, of the direct tar work.

The cadence benchmark identified ordered gzip output as the main overlap limit. With a 1 MiB speculative grid, output began at 8.491 ms, reached 25% at 29.595 ms, and completed at 33.724 ms. A 512 KiB grid began at 4.798 ms, reached 25% at 23.993 ms, and completed at 32.915 ms. A 256 KiB grid emitted earlier but slowed raw decode to 38.073 ms and extraction to 57.792 ms. The 512 KiB grid gave the best measured balance. Computing each clean suffix CRC in its primary job moved 25% output to 19.818 ms, 75% to 30.355 ms, and completion to 30.678 ms. The corresponding extraction run was effectively flat. Tar cannot process later bytes while an earlier ordered gzip segment remains incomplete. A custom tar parser would not remove that dependency. A one-chunk channel buffer regressed extraction to 59.698 ms, so the bridge retains its zero-capacity rendezvous.

Raw-tar output is a lower bound rather than an extractor reference. The fbz tests use a broad 3x raw-decode regression guard. Keep the measurements single-run; change an implementation before rerunning it.

### Local ZIP benchmark reproduction

`tests/zip_perf.rs` creates two deterministic ZIPs from the same 84,423,012-byte SimpleWiki prefix used by the tar benchmarks: one DEFLATE entry, and 18 equal-sized DEFLATE entries. `tests/support::simplewiki_prefix` decodes `meta/simplewiki-first-5pct.xml.bz2` before timing, and the ZIP builder then runs before timing. Thus regenerating the dataset is exactly the SimpleWiki 5% procedure below; no ZIP fixture is stored or needs hand-maintained lengths.

Each speed test warms its selected executable once, measures it once, and verifies all extracted bytes. Run only the row whose implementation changed:

```bash
cargo test --release --test zip_perf zip_single_fbz -- --ignored --exact --nocapture
cargo test --release --test zip_perf zip_single_unzip -- --ignored --exact --nocapture
cargo test --release --test zip_perf zip_many_fbz -- --ignored --exact --nocapture
cargo test --release --test zip_perf zip_many_unzip -- --ignored --exact --nocapture
```

`FBZ_THREADS` is an optional diagnostic override; normal tests and benchmarks should leave it unset so `-P 0` follows the machine automatically.

The ungated child-process memory diagnostics use the same many-entry fixture:

```bash
cargo test --release --test zip_perf zip_many_fbz_process_metrics -- --ignored --exact --nocapture
cargo test --release --test zip_perf zip_many_unzip_process_metrics -- --ignored --exact --nocapture
```

These diagnostics quantify the memory cost of concurrent entry decoding without duplicating the user-facing results.

### Local Wikipedia benchmarks

`tests/wiki_perf.rs` contains release-mode local benchmarks that are skipped by default. The git-ignored SimpleWiki fixtures are a bzip2-compressed 5% prefix, the full bzip2 dump, the same 5% prefix recompressed as gzip, and the full XML recompressed with system `gzip -6`.

This section retains in-process and library-oriented research comparisons that are useful to implementation work but deliberately excluded from the user-facing README. Full Simple English Wikipedia (`338 MB` compressed, `1,688,460,257` bytes decoded):

| Decoder | Mode | Seconds |
|---|---|---:|
| fbz | parallel, 18 threads, streaming sink | 2.515 |
| crabz2 0.4.0 | parallel | 4.460 |
| bzip2 | serial CLI | 20.310 |
| pbzip2 1.1.13 | CLI | 20.240 |
| libbz2-rs 0.2.5 | serial, in process | 20.700 |
| fbz | serial, in process | 21.279 |

The first 1,000 streams of English Wikipedia (`654,362,682` bytes compressed, `2,715,335,085` bytes decoded, 99,853 pages) exercise scheduling across many short concatenated streams:

| Decoder | Mode | Seconds |
|---|---|---:|
| crabz2 0.4.0 | parallel, in process | 3.815 |
| fbz | parallel, 18 threads, in process | 3.881 |
| fbz | serial, in process | 37.198 |
| crabz2 0.4.0 | serial, in process | 40.602 |
| pbzip2 1.1.13 | 18-thread CLI + byte comparison | 88.080 |
| bzip2 | serial CLI + byte comparison | 92.960 |

The last two rows stream 2.5 GB through `cmp` against the validated XML, so their absolute times are not directly comparable with the in-process rows. The tables record distinct implementation experiments, not a user-facing CLI ranking.

The quick bzip2 iteration test reads `meta/simplewiki-first-5pct.xml.bz2` before timing, then verifies decoded length, all CRCs, and BLAKE3:

```bash
cargo test --release --test wiki_perf simplewiki_first_five_percent -- --ignored --exact --nocapture
```

The full bzip2 confirmation streams to a counting sink and validates every block and stream CRC:

```bash
cargo test --release --test wiki_perf simplewiki_full -- --ignored --exact --nocapture
```

The fbz-only gzip test warms with `meta/simplewiki-first-5pct.xml.gz` and performs one full-dump validation. The ratio test warms both executables, measures each full dump once, and fails above 1.2x the sibling rapidgzip-rust checkout:

```bash
cargo test --release --test wiki_perf gzip_fbz_validation -- --ignored --exact --nocapture
cargo test --release --test wiki_perf gzip_reference_ratio -- --ignored --exact --nocapture
```

The default selects available parallelism automatically. `FBZ_THREADS` is an optional diagnostic override, and `RAPIDGZIP_BIN` can point at another reference executable. The warm-up is deliberately the small fixture, not an unreported repeat of the measured full workload.

Time/CPU/RSS and ungated macOS physical-footprint diagnostics are separate because process inspection can perturb sub-second parallel timings:

```bash
cargo test --release --test wiki_perf gzip_cli_process_metrics -- --ignored --exact --nocapture
cargo test --release --test wiki_perf rapidgzip_rust_process_metrics -- --ignored --exact --nocapture
cargo test --release --test wiki_perf system_gzip_process_metrics -- --ignored --exact --nocapture
```

The metrics helper uses `wait4` and, on macOS, `proc_pid_rusage`'s `ri_phys_footprint` field on its own child; it needs no task-inspection permission. Wall time stops as soon as `wait4` reports child exit, before joining the 50 ms footprint sampler, so short-process timings do not include sampler shutdown latency. The 1,000-stream enwiki comparison has a separate ignored test for each implementation and mode so a changed decoder can be measured without rerunning unchanged baselines. Each test reads the compressed fixture before starting its single timed decode, with no warmups or repeats:

```bash
cargo test --release --test wiki_perf enwiki_first_1000_fbz_parallel -- --ignored --exact --nocapture
cargo test --release --test wiki_perf enwiki_first_1000_crabz2_parallel -- --ignored --exact --nocapture
cargo test --release --test wiki_perf enwiki_first_1000_fbz_serial -- --ignored --exact --nocapture
cargo test --release --test wiki_perf enwiki_first_1000_crabz2_serial -- --ignored --exact --nocapture
```

It compares fbz and crabz2 in parallel and serial modes. Automatic parallelism is the default; `FBZ_THREADS` can give both parallel decoders an explicit count for a diagnostic comparison. Each implementation validates the bzip2 CRCs; the benchmark also checks the exact decoded length.

The decoded lengths and the 5% BLAKE3 in `tests/wiki_perf.rs` are acceptance values, not parameters used by the decoder. They prevent a truncated decode from appearing artificially fast without reading a separate multi-gigabyte reference during each timed run. Regenerating a fixture requires independently validating it and updating the corresponding acceptance value.

#### Regenerating the fixtures

Set `wiki` to the checkout containing the Wikimedia dumps:

```bash
wiki=/path/to/parse-wiki
```

Recreate the SimpleWiki fixtures from its validated compressed and decoded files:

```bash
head -c 84423012 "$wiki/data/simplewiki-latest-pages-articles.xml" | bzip2 -9c > meta/simplewiki-first-5pct.xml.bz2
ln -s "$wiki/data/simplewiki-latest-pages-articles.xml.bz2" meta/simplewiki-full.xml.bz2
head -c 84423012 "$wiki/data/simplewiki-latest-pages-articles.xml" | gzip -6c > meta/simplewiki-first-5pct.xml.gz
gzip -6c "$wiki/data/simplewiki-latest-pages-articles.xml" > meta/simplewiki-full.xml.gz
```

Run the 5% test to obtain and verify its decoded length and BLAKE3. Obtain the full decoded length with `stat`; update `FIVE_PERCENT_LEN`, `FIVE_PERCENT_BLAKE3`, or `FULL_LEN` only when intentionally changing a fixture.

The enwiki dump is multistream. Extract its unique compressed stream offsets from the official index, whose first page-bearing stream begins after an unindexed initial stream at byte zero:

```bash
bzcat "$wiki/data/enwiki-latest-pages-articles-multistream-index.txt.bz2" \
  | awk -F: '!seen[$1]++ {print $1}' > "$wiki/meta/enwiki-multistream-offsets.txt"
boundary=$(sed -n '1000p' "$wiki/meta/enwiki-multistream-offsets.txt")
head -c "$boundary" "$wiki/data/enwiki-latest-pages-articles-multistream.xml.bz2" \
  > "$wiki/data/enwiki-first-1000-streams.xml.bz2"
ln -s "$wiki/data/enwiki-first-1000-streams.xml.bz2" meta/enwiki-first-1000-streams.xml.bz2
```

Line 1000 is the start of stream 1001 because byte zero is stream 1 and is absent from the page index. Thus `[0, boundary)` contains exactly 1,000 complete bzip2 streams. For the 2026-08-01 dump, `boundary` is `654362682` and the decoded bzip2 payload length is `2715335085`.

To create the separately useful well-formed parser fixture, append only the XML root close after decoding; those 13 bytes are deliberately excluded from `ENWIKI_1000_LEN`:

```bash
fbz "$wiki/data/enwiki-first-1000-streams.xml.bz2" -o "$wiki/data/enwiki-first-1000-streams.xml"
printf '</mediawiki>\n' >> "$wiki/data/enwiki-first-1000-streams.xml"
xmllint --stream --noout "$wiki/data/enwiki-first-1000-streams.xml"
```

## Platforms

CI tests and builds Linux on x86-64 and ARM64, and macOS on ARM64. macOS Intel remains best-effort and should not add implementation complexity. Keep the core portable: no required mmap, custom allocator, `io_uring`, assembly, or native-endian parsing. Platform-specific positional I/O belongs behind a small source abstraction.

## Versioning

The canonical version lives in `Cargo.toml`. `pyproject.toml` gets the Python package version from Cargo via `dynamic = ["version"]`.

The thin PEP 517 backend delegates to Maturin after building and staging the native executable, so repository and editable builds contain the same native CLI as CI-built wheels. Ordinary builds use the incremental `release` profile; CI builds distributed wheels and executables with the full-LTO, single-codegen-unit, stripped `dist` profile. Releases publish platform wheels rather than a Python sdist; the Rust source package is published separately to crates.io.

## Release

1. Run `cargo build --release --bins && python tools/stage_binaries.py`.
2. Run `uv pip install --reinstall --no-deps -e . && pytest -q` so the custom backend installs both the extension and native CLI.
3. Confirm the release version in `Cargo.toml` (`[package].version`).
4. For the first crates.io release only, run `cargo publish`, then configure the `ci.yml` trusted publisher for `AnswerDotAI/fbz`; crates.io requires the crate to exist before trusted publishing can be configured.
5. Run `ship-release`.

Fastship pushes the version tag for GitHub Actions, then bumps and pushes `Cargo.toml`. Tagged CI publishes the crate through crates.io trusted publishing as well as building the GitHub release and PyPI packages. The AnswerDotAI Homebrew tap discovers the new crate version in its daily bump workflow; green bot-created fbz-only formula PRs publish their bottles automatically through the tap's generated `brew pr-pull` workflow.
