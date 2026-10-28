import bz2, gzip, io
from threading import Event, Thread

import pytest

import fbz

def patterned(size): return bytes((i * 37 + i // 251) & 255 for i in range(size))

@pytest.mark.parametrize("level", range(1, 10))
def test_decompress_matches_libbz2_at_every_level(level):
    plain = patterned(20_000)
    assert fbz.decompress(bz2.compress(plain, compresslevel=level), threads=2) == plain

def test_decompress_detects_every_stream_format():
    plain = patterned(300_000)
    encoded = [bz2.compress(plain), gzip.compress(plain), fbz.compress(plain, "lz4")]
    for item in encoded: assert fbz.decompress(item, threads=2) == plain
    assert fbz.decompress(encoded[1], "gzip", threads=2) == plain
    with pytest.raises(ValueError, match="format"): fbz.decompress(encoded[0], "zip")

def test_compress_stream_formats_and_options():
    plain = patterned(300_000)
    assert bz2.decompress(fbz.compress(plain, "bzip2", threads=2, level=3)) == plain
    assert gzip.decompress(fbz.compress(plain, "gzip", threads=2, level=6)) == plain
    assert fbz.compress(plain, "lz4", threads=2)[:4] == b"\x04\x22\x4d\x18"
    with pytest.raises(ValueError, match="format"): fbz.compress(plain, "zip")

def test_parallel_multiblock_and_concatenated_are_deterministic():
    first, second = patterned(350_000), patterned(75_000)
    compressed = bz2.compress(first, compresslevel=1) + bz2.compress(second, compresslevel=9)
    expected = first + second
    for threads in (1, 2, 4, 0): assert fbz.decompress(compressed, threads=threads) == expected

def test_bad_crc_raises_package_error():
    compressed = bytearray(bz2.compress(b"integrity matters"))
    compressed[-2] ^= 1
    with pytest.raises(fbz.BadCompressedFile): fbz.decompress(bytes(compressed))

def test_seekable_file_and_persisted_index(tmp_path):
    plain = patterned(350_000)
    compressed = bz2.compress(plain, compresslevel=1)
    source = tmp_path / "data.bz2"
    index_path = tmp_path / "data.fbz2i"
    source.write_bytes(compressed)
    encoded = fbz.build_index(source, index_path, threads=2)

    with fbz.open_indexed(source, index=index_path) as handle:
        assert handle.size == len(plain)
        assert handle.seek(99_990) == 99_990
        assert handle.read(40) == plain[99_990:100_030]
        handle.seek(-17, io.SEEK_END)
        target = bytearray(20)
        assert handle.readinto(target) == 17
        assert target[:17] == plain[-17:]
        assert handle.index_bytes() == encoded
    assert handle.closed

def test_index_is_bound_to_source():
    first = bz2.compress(b"first")
    second = bz2.compress(b"other")
    index = fbz.build_index(first)
    with pytest.raises(ValueError, match="source identity mismatch"): fbz.open_indexed(second, index=index)

@pytest.mark.parametrize("format", ["bzip2", "gzip", "lz4"])
def test_buffered_reader_compatibility(tmp_path, format):
    plain = patterned(180_000)
    source = tmp_path / f"data.{format}"
    source.write_bytes(fbz.compress(plain, format, threads=2))
    with io.BufferedReader(fbz.open(source, threads=2)) as handle:
        assert handle.read(1234) == plain[:1234]
        assert handle.read() == plain[1234:]
    assert handle.closed

def test_stream_reader_readinto_and_explicit_format(tmp_path):
    plain = patterned(2_000_000)
    source = tmp_path / "misleading.bz2"
    source.write_bytes(gzip.compress(plain))
    target = bytearray(len(plain) + 100)
    with fbz.Reader(source, format="gzip", threads=2) as handle:
        assert handle.readinto(bytearray()) == 0
        count = handle.readinto(memoryview(target)[17:])
        assert count <= 1024 * 1024
        assert target[17:17 + count] == plain[:count]
        assert target[:17] == bytes(17) and target[17 + count:] == bytes(len(target) - 17 - count)
        assert bytes(target[17:17 + count]) + handle.read() == plain

@pytest.mark.parametrize("target", [b"----", memoryview(bytearray(8))[::2]])
def test_stream_reader_rejects_readonly_and_noncontiguous_buffers(tmp_path, target):
    source = tmp_path / "data.gz"
    source.write_bytes(gzip.compress(b"data"))
    with fbz.Reader(source) as handle:
        with pytest.raises(TypeError, match="writable, contiguous"): handle.readinto(target)
        assert handle.read() == b"data"

def test_stream_reader_reports_late_checksum_failure(tmp_path):
    plain = patterned(1_000_000)
    encoded = bytearray(gzip.compress(plain))
    encoded[-8] ^= 1
    source = tmp_path / "corrupt.gz"
    source.write_bytes(encoded)
    with fbz.open(source, threads=2) as handle:
        with pytest.raises(fbz.BadCompressedFile): handle.read()

def test_validates_all_stream_formats_from_paths_and_bytes(tmp_path):
    plain = patterned(300_000)
    for format in ("bzip2", "gzip", "lz4"):
        encoded = fbz.compress(plain, format, threads=2)
        source = tmp_path / f"data.{format}"
        source.write_bytes(encoded)
        assert fbz.test(encoded, threads=2) is None
        assert fbz.test(source, threads=2) is None

def test_native_decode_releases_gil():
    plain = patterned(2_000_000)
    compressed = bz2.compress(plain, compresslevel=1)
    started, stop = Event(), Event()
    counter = [0]

    def spin():
        started.set()
        while not stop.is_set(): counter[0] += 1

    thread = Thread(target=spin)
    thread.start()
    started.wait()
    before = counter[0]
    try: assert fbz.decompress(compressed, threads=2) == plain
    finally:
        stop.set()
        thread.join()
    assert counter[0] > before
