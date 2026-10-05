use std::{
    io::{Read, Seek, SeekFrom},
    sync::Mutex,
};

use pyo3::{
    buffer::PyBuffer,
    create_exception,
    exceptions::{PyOSError, PyTypeError, PyValueError},
    prelude::*,
    types::PyBytes,
};

create_exception!(fbz, BadCompressedFile, PyOSError);

fn decode_format(format: Option<&str>) -> PyResult<fbz::DecodeFormat> {
    match format {
        None | Some("auto") => Ok(fbz::DecodeFormat::Auto),
        Some("bzip2") => Ok(fbz::DecodeFormat::Bzip2),
        Some("gzip") => Ok(fbz::DecodeFormat::Gzip),
        Some("lz4") => Ok(fbz::DecodeFormat::Lz4),
        Some(_) => Err(PyValueError::new_err("format must be 'bzip2', 'gzip', 'lz4', or None")),
    }
}

fn decode_options(format: Option<&str>, threads: usize, memory_limit: usize) -> PyResult<fbz::DecodeOptions> {
    Ok(fbz::DecodeOptions { format: decode_format(format)?, threads, memory_limit })
}

fn python_error(error: fbz::Error) -> PyErr {
    match error {
        fbz::Error::Io(source) => PyOSError::new_err(source.to_string()),
        fbz::Error::InvalidConfiguration(message) | fbz::Error::InvalidIndex(message) => PyValueError::new_err(message),
        error => BadCompressedFile::new_err(error.to_string()),
    }
}

fn python_read_error(error: std::io::Error) -> PyErr {
    match error.kind() {
        std::io::ErrorKind::InvalidData => BadCompressedFile::new_err(error.to_string()),
        std::io::ErrorKind::InvalidInput => PyValueError::new_err(error.to_string()),
        _ => PyOSError::new_err(error.to_string()),
    }
}

#[pyfunction(name = "_scan")]
#[allow(clippy::type_complexity)]
fn py_scan(data: &[u8]) -> PyResult<(Vec<(u64, u8)>, Vec<(u64, u32, bool, u32)>, Vec<(u64, u32)>)> {
    let result = fbz::scan(data).map_err(|err| PyValueError::new_err(err.to_string()))?;
    let streams = result.streams.into_iter().map(|stream| (stream.byte_offset, stream.block_size_100k)).collect();
    let blocks = result.blocks.into_iter().map(|block| (block.bit_offset, block.expected_crc, block.randomized, block.orig_ptr)).collect();
    let stream_ends = result.stream_ends.into_iter().map(|end| (end.bit_offset, end.expected_stream_crc)).collect();
    Ok((streams, blocks, stream_ends))
}

#[pyfunction(name = "bz2_crc32")]
fn py_bz2_crc32(data: &[u8]) -> u32 { fbz::bz2_crc32(data) }

#[pyfunction(name = "_decompress", signature = (data, format=None, threads=0, memory_limit=fbz::DEFAULT_MEMORY_LIMIT))]
fn py_decompress(py: Python<'_>, data: &[u8], format: Option<&str>, threads: usize, memory_limit: usize) -> PyResult<Py<PyBytes>> {
    let options = decode_options(format, threads, memory_limit)?;
    let data = data.to_vec();
    let output = py.detach(move || fbz::decompress(&data, options)).map_err(python_error)?;
    Ok(PyBytes::new(py, &output).unbind())
}

#[pyfunction(name = "_compress", signature = (data, format, threads=0, memory_limit=fbz::DEFAULT_MEMORY_LIMIT, level=None))]
fn py_compress(py: Python<'_>, data: &[u8], format: &str, threads: usize, memory_limit: usize, level: Option<u8>) -> PyResult<Py<PyBytes>> {
    let format = match format {
        "bzip2" => fbz::EncodeFormat::Bzip2,
        "gzip" => fbz::EncodeFormat::Gzip,
        "lz4" => fbz::EncodeFormat::Lz4,
        _ => return Err(PyValueError::new_err("format must be 'bzip2', 'gzip', or 'lz4'")),
    };
    let data = data.to_vec();
    let output = py.detach(move || fbz::compress(&data, format, fbz::EncodeOptions { threads, memory_limit, level })).map_err(python_error)?;
    Ok(PyBytes::new(py, &output).unbind())
}

#[pyfunction(name = "_build_index", signature = (path, threads=0, memory_limit=fbz::DEFAULT_MEMORY_LIMIT))]
fn py_build_index(py: Python<'_>, path: String, threads: usize, memory_limit: usize) -> PyResult<Py<PyBytes>> {
    let encoded = py
        .detach(move || {
            let source = fbz::Source::open(path)?;
            Ok::<_, fbz::Error>(fbz::build_index(source.as_slice(), fbz::DecodeOptions { threads, memory_limit, ..fbz::DecodeOptions::default() })?.to_bytes())
        })
        .map_err(python_error)?;
    Ok(PyBytes::new(py, &encoded).unbind())
}

#[pyfunction(name = "_test_path", signature = (path, format=None, threads=0, memory_limit=fbz::DEFAULT_MEMORY_LIMIT))]
fn py_test_path(py: Python<'_>, path: String, format: Option<&str>, threads: usize, memory_limit: usize) -> PyResult<()> {
    let options = decode_options(format, threads, memory_limit)?;
    py.detach(move || {
        let source = fbz::Source::open(&path)?;
        let stream_format = options.format.detect_path(std::path::Path::new(&path), source.as_slice())?;
        let mut output = fbz::WriterSink::new(std::io::sink());
        fbz::decode_stream_to_sink_with_progress(stream_format, source.as_slice(), &mut output, options, |_| {})
    })
    .map_err(python_error)
}

#[pyfunction(name = "_test_bytes", signature = (data, format=None, threads=0, memory_limit=fbz::DEFAULT_MEMORY_LIMIT))]
fn py_test_bytes(py: Python<'_>, data: &[u8], format: Option<&str>, threads: usize, memory_limit: usize) -> PyResult<()> {
    let options = decode_options(format, threads, memory_limit)?;
    let data = data.to_vec();
    py.detach(move || fbz::decompress_to_writer(&data, &mut std::io::sink(), options)).map_err(python_error)
}

#[pyclass(name = "_Reader")]
struct PyReader { inner: Mutex<fbz::Reader> }

const PY_READINTO_CHUNK: usize = 1024 * 1024;

#[pymethods]
impl PyReader {
    #[staticmethod]
    #[pyo3(signature = (path, format=None, threads=0, memory_limit=fbz::DEFAULT_MEMORY_LIMIT))]
    fn from_path(path: String, format: Option<&str>, threads: usize, memory_limit: usize) -> PyResult<Self> {
        let options = decode_options(format, threads, memory_limit)?;
        Ok(Self { inner: Mutex::new(fbz::Reader::open(path, options).map_err(python_error)?) })
    }

    fn read(&self, py: Python<'_>, size: i64) -> PyResult<Py<PyBytes>> {
        let output = py
            .detach(|| {
                let mut reader = self.inner.lock().map_err(|_| std::io::Error::other("reader lock poisoned"))?;
                if size < 0 {
                    let mut output = Vec::new();
                    reader.read_to_end(&mut output)?;
                    Ok(output)
                } else {
                    let count = usize::try_from(size)
                        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "requested read does not fit this platform"))?;
                    let mut output = vec![0; count];
                    let count = reader.read(&mut output)?;
                    output.truncate(count);
                    Ok(output)
                }
            })
            .map_err(python_read_error)?;
        Ok(PyBytes::new(py, &output).unbind())
    }

    fn readinto(&self, py: Python<'_>, buffer: PyBuffer<u8>) -> PyResult<usize> {
        if buffer.as_mut_slice(py).is_none() { return Err(PyTypeError::new_err("readinto() requires a writable, contiguous byte buffer")); }
        let mut output = vec![0; buffer.item_count().min(PY_READINTO_CHUNK)];
        let count = py
            .detach(|| { let mut reader = self.inner.lock().map_err(|_| std::io::Error::other("reader lock poisoned"))?; reader.read(&mut output) })
            .map_err(python_read_error)?;
        for (destination, byte) in buffer.as_mut_slice(py).unwrap()[..count].iter().zip(&output) { destination.set(*byte); }
        Ok(count)
    }
}

#[pyclass(name = "_IndexedReader")]
struct PyIndexedReader { inner: Mutex<fbz::IndexedReader> }

#[pymethods]
impl PyIndexedReader {
    #[staticmethod]
    #[pyo3(signature = (path, threads=0, memory_limit=fbz::DEFAULT_MEMORY_LIMIT, index_path=None, cache_limit=fbz::DEFAULT_CACHE_LIMIT))]
    fn from_path(py: Python<'_>, path: String, threads: usize, memory_limit: usize, index_path: Option<String>, cache_limit: usize) -> PyResult<Self> {
        let inner = py
            .detach(move || match index_path {
                Some(index_path) => fbz::IndexedReader::open_with_index(path, index_path, cache_limit),
                None => fbz::IndexedReader::from_source(
                    fbz::Source::open(path)?,
                    None,
                    fbz::DecodeOptions { threads, memory_limit, ..fbz::DecodeOptions::default() },
                    cache_limit,
                ),
            })
            .map_err(python_error)?;
        Ok(Self { inner: Mutex::new(inner) })
    }

    #[staticmethod]
    #[pyo3(signature = (data, threads=0, memory_limit=fbz::DEFAULT_MEMORY_LIMIT, index=None, cache_limit=fbz::DEFAULT_CACHE_LIMIT))]
    fn from_bytes(py: Python<'_>, data: &[u8], threads: usize, memory_limit: usize, index: Option<&[u8]>, cache_limit: usize) -> PyResult<Self> {
        let data = data.to_vec();
        let index = index.map(<[u8]>::to_vec);
        let inner = py
            .detach(move || match index {
                Some(index) => fbz::IndexedReader::from_bytes_with_index(data, &index, cache_limit),
                None => fbz::IndexedReader::from_source(
                    fbz::Source::from_bytes(data),
                    None,
                    fbz::DecodeOptions { threads, memory_limit, ..fbz::DecodeOptions::default() },
                    cache_limit,
                ),
            })
            .map_err(python_error)?;
        Ok(Self { inner: Mutex::new(inner) })
    }

    fn read(&self, py: Python<'_>, size: i64) -> PyResult<Py<PyBytes>> {
        let output = py
            .detach(|| {
                let mut reader = self.inner.lock().map_err(|_| fbz::Error::InvalidConfiguration("reader lock poisoned".into()))?;
                let remaining = reader.size() - reader.position();
                let count = if size < 0 { remaining } else { remaining.min(size as u64) };
                let count = usize::try_from(count).map_err(|_| fbz::Error::InvalidConfiguration("requested read does not fit this platform".into()))?;
                let mut output = vec![0; count];
                reader.read_exact(&mut output)?;
                Ok::<_, fbz::Error>(output)
            })
            .map_err(python_error)?;
        Ok(PyBytes::new(py, &output).unbind())
    }

    #[pyo3(signature = (offset, whence=0))]
    fn seek(&self, py: Python<'_>, offset: i64, whence: i32) -> PyResult<u64> {
        py.detach(|| {
            let mut reader = self.inner.lock().map_err(|_| fbz::Error::InvalidConfiguration("reader lock poisoned".into()))?;
            let position = match whence {
                0 if offset >= 0 => SeekFrom::Start(offset as u64),
                0 => return Err(fbz::Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "negative absolute seek"))),
                1 => SeekFrom::Current(offset),
                2 => SeekFrom::End(offset),
                _ => return Err(fbz::Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid whence"))),
            };
            reader.seek(position).map_err(fbz::Error::from)
        })
        .map_err(python_error)
    }

    fn tell(&self) -> PyResult<u64> { self.inner.lock().map(|reader| reader.position()).map_err(|_| PyValueError::new_err("reader lock poisoned")) }

    #[getter]
    fn size(&self) -> PyResult<u64> { self.inner.lock().map(|reader| reader.size()).map_err(|_| PyValueError::new_err("reader lock poisoned")) }

    fn index_bytes(&self, py: Python<'_>) -> PyResult<Py<PyBytes>> {
        let encoded = self.inner.lock().map(|reader| reader.index().to_bytes()).map_err(|_| PyValueError::new_err("reader lock poisoned"))?;
        Ok(PyBytes::new(py, &encoded).unbind())
    }
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(py_scan, m)?)?;
    m.add_function(wrap_pyfunction!(py_bz2_crc32, m)?)?;
    m.add_function(wrap_pyfunction!(py_decompress, m)?)?;
    m.add_function(wrap_pyfunction!(py_compress, m)?)?;
    m.add_function(wrap_pyfunction!(py_build_index, m)?)?;
    m.add_function(wrap_pyfunction!(py_test_path, m)?)?;
    m.add_function(wrap_pyfunction!(py_test_bytes, m)?)?;
    m.add_class::<PyReader>()?;
    m.add_class::<PyIndexedReader>()?;
    m.add("BadCompressedFile", m.py().get_type::<BadCompressedFile>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
