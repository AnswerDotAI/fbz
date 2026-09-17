#[path = "fbz/archive_create.rs"]
mod archive_create;
#[path = "fbz/archive_extract.rs"]
mod archive_extract;
#[path = "fbz/tar_create.rs"]
mod tar_create;
#[path = "fbz/tar_extract.rs"]
mod tar_extract;
#[path = "fbz/zip_extract.rs"]
mod zip_extract;

use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use fbz::{
    DecodeOptions, DecodeProgress, EncodeOptions, EncodeProgress, Error, Format, Index, OutputSink, Source, WriterSink, build_index_with_progress,
    decode_stream_to_sink_with_progress, decode_to_writer_with_progress, gzip, lz4,
};
use serde_json::{Value, json};
use tempfile::NamedTempFile;

#[derive(Parser)]
#[command(
    version,
    about = "Fast parallel compression and decompression with safe archive handling",
    long_about = "Parallel compression and decompression for bzip2, gzip, and LZ4, plus compressed-tar and ZIP archives. A path without a command defaults to decompression.",
    after_help = r#"Run fbz <command> -h for its full options (e.g. fbz c -h, fbz d -h).

Examples:
  fbz dump.xml.bz2                       Write dump.xml (implicit decompression)
  fbz d backup.tgz -C restored           Extract into restored/
  fbz c src docs -o source.zip -i -E .git Create a filtered archive
  fbz c src -o src.tar.gz -ni            Preview archive contents
  fbz l --json data.gz                   Show the validated layout as JSON
  fbz test archive.tar.bz2               Validate without writing output
  fbz index dump.xml.bz2                 Build a bzip2 seek index"#
)]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Common {
    /// Compression or decompression workers; 0 selects automatically.
    #[arg(short = 'P', long, default_value_t = 0, global = true)]
    threads: usize,
    /// Maximum in-flight codec memory budget; accepts binary size suffixes.
    #[arg(long, default_value = "1G", value_parser = parse_size, global = true)]
    memory_limit: usize,
    /// Suppress progress, skip notices, and the dry-run summary.
    #[arg(short, long, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Compress files or create a tar/ZIP archive.
    /// Output (-o), format, level (-l), filters (-i/-E/-e), dry-run (-n)
    #[command(visible_alias = "c", verbatim_doc_comment)]
    Compress(CompressArgs),
    /// Decompress files or extract archives; also the default for bare paths.
    /// Output (-o/-C), extraction (-x), overwrite, removal, size limit
    #[command(visible_alias = "d", verbatim_doc_comment)]
    Decompress(DecompressArgs),
    /// Validate and list codec structure or ZIP entries.
    /// JSON (--json), size limit (--max-output)
    #[command(visible_alias = "l", verbatim_doc_comment)]
    List(ListArgs),
    /// Fully decode and validate checksums without writing output.
    /// Size limit (--max-output)
    #[command(verbatim_doc_comment)]
    Test(TestArgs),
    /// Build validated, source-bound .fbz2i indexes for bzip2 inputs.
    /// Output (-o), overwrite, skip existing, size limit
    #[command(verbatim_doc_comment)]
    Index(IndexArgs),
}

#[derive(Args)]
struct WriteArgs {
    /// Write to PATH or stdout (-).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Replace existing output files or archive entries.
    #[arg(short, long, conflicts_with = "skip_existing")]
    force: bool,
    /// Skip existing output files; unavailable during extraction.
    #[arg(long)]
    skip_existing: bool,
}

#[derive(Args)]
struct CompressArgs {
    /// Input files, or - for a standalone byte stream.
    #[arg(required = true, num_args = 1..)]
    inputs: Vec<String>,
    #[command(flatten)]
    write: WriteArgs,
    /// Compression format; inferred from -o when possible.
    #[arg(long, value_enum)]
    format: Option<CompressionFormat>,
    /// Compression level from 1 (fastest) through 9 (smallest); format-specific default.
    #[arg(short = 'l', long)]
    level: Option<u8>,
    /// Put generated files in DIRECTORY.
    #[arg(short = 'C', long = "output-dir", conflicts_with = "output")]
    output_dir: Option<PathBuf>,
    /// Remove inputs after successful standalone compression; not archive creation.
    #[arg(long = "rm")]
    remove_input: bool,
    #[command(flatten)]
    filters: archive_create::Filters,
}

#[derive(Args)]
struct DecompressArgs {
    /// Input files, or - for stdin.
    #[arg(required = true, num_args = 1..)]
    inputs: Vec<String>,
    #[command(flatten)]
    write: WriteArgs,
    /// Put generated files or extracted archive entries in DIRECTORY.
    #[arg(short = 'C', long = "output-dir", conflicts_with = "output")]
    output_dir: Option<PathBuf>,
    /// Force tar or ZIP extraction for stdin or an unusual filename.
    #[arg(short = 'x', long, conflicts_with_all = ["output", "skip_existing"])]
    extract: bool,
    /// Remove inputs after successful decoding or extraction.
    #[arg(long = "rm")]
    remove_input: bool,
    /// Maximum decoded bytes per input; accepts binary size suffixes.
    #[arg(long, value_parser = parse_size)]
    max_output: Option<usize>,
}

#[derive(Args)]
struct ListArgs {
    /// Input files to validate and list.
    #[arg(required = true, num_args = 1..)]
    inputs: Vec<String>,
    /// Emit the validated layout as JSON.
    #[arg(long)]
    json: bool,
    /// Maximum decoded bytes per input; accepts binary size suffixes.
    #[arg(long, value_parser = parse_size)]
    max_output: Option<usize>,
}

#[derive(Args)]
struct TestArgs {
    /// Input files, or - for stdin.
    #[arg(required = true, num_args = 1..)]
    inputs: Vec<String>,
    /// Maximum decoded bytes per input; accepts binary size suffixes.
    #[arg(long, value_parser = parse_size)]
    max_output: Option<usize>,
}

#[derive(Args)]
struct IndexArgs {
    /// Bzip2 input files.
    #[arg(required = true, num_args = 1..)]
    inputs: Vec<String>,
    #[command(flatten)]
    write: WriteArgs,
    /// Maximum decoded bytes per input; accepts binary size suffixes.
    #[arg(long, value_parser = parse_size)]
    max_output: Option<usize>,
}

// Find the first positional without mistaking an option value for a command.
// Clap's schema is the source of truth for both aliases and value-taking options.
fn cli_args() -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = std::env::args_os().collect();
    let mut schema = Cli::command();
    schema.build();
    let flags: Vec<_> = schema.get_arguments().chain(schema.get_subcommands().flat_map(|cmd| cmd.get_arguments())).collect();
    let mut skip_value = false;
    let mut implicit = args.len() > 1;
    for arg in &args[1..] {
        if skip_value { skip_value = false; continue; }
        let text = arg.to_string_lossy();
        if matches!(text.as_ref(), "-h" | "--help" | "-V" | "--version") { implicit = false; break; }
        if text == "--" { break; }
        if let Some(long) = text.strip_prefix("--") {
            skip_value = flags.iter().any(|flag| flag.get_long() == Some(long) && flag.get_action().takes_values());
        } else if let Some(short) = text.strip_prefix('-').filter(|short| !short.is_empty()) {
            let mut chars = short.chars().peekable();
            while let Some(ch) = chars.next() {
                if flags.iter().any(|flag| flag.get_short() == Some(ch) && flag.get_action().takes_values()) {
                    skip_value = chars.peek().is_none();
                    break;
                }
            }
        } else {
            implicit = !schema.get_subcommands().any(|cmd| cmd.get_name() == text || cmd.get_all_aliases().any(|alias| alias == text));
            break;
        }
    }
    if implicit { args.insert(1, "decompress".into()); }
    args
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CompressionFormat {
    Bzip2,
    Gzip,
    Lz4,
    TarBzip2,
    TarGzip,
    TarLz4,
    Zip,
}

fn main() -> ExitCode {
    match run(Cli::parse_from(cli_args())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fbz: {error}");
            ExitCode::from(exit_status(&error))
        }
    }
}

fn run(cli: Cli) -> fbz::Result<()> {
    let common = &cli.common;
    let options = DecodeOptions { threads: common.threads, memory_limit: common.memory_limit, ..DecodeOptions::default() };
    match &cli.command {
        Command::Compress(args) => run_compress(args, common),
        Command::Decompress(args) => run_decode(args, options, common.quiet),
        Command::List(args) => run_list(args, options, common.quiet),
        Command::Index(args) => run_index(args, options, common.quiet),
        Command::Test(args) => {
            validate_inputs(&args.inputs, true)?;
            for input in &args.inputs { test_input(input, options, args.max_output, common.quiet)?; }
            Ok(())
        }
    }
}

fn validate_inputs(inputs: &[String], stdin: bool) -> fbz::Result<()> {
    if inputs.iter().any(|input| input == "-") {
        if !stdin { return Err(invalid("stdin is not supported for this command")); }
        if inputs.len() != 1 { return Err(invalid("stdin must be the only input")); }
    }
    Ok(())
}

fn inferred_compression_format(path: &Path) -> Option<CompressionFormat> {
    let name = path.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".tar.bz2") || name.ends_with(".tar.bzip2") || name.ends_with(".tbz") || name.ends_with(".tbz2") {
        Some(CompressionFormat::TarBzip2)
    } else if name.ends_with(".tar.lz4") {
        Some(CompressionFormat::TarLz4)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tar.gzip") || name.ends_with(".tgz") {
        Some(CompressionFormat::TarGzip)
    } else if name.ends_with(".zip") { Some(CompressionFormat::Zip) } else if name.ends_with(".gz") || name.ends_with(".gzip") { Some(CompressionFormat::Gzip) } else if name.ends_with(".bz2") || name.ends_with(".bzip2") { Some(CompressionFormat::Bzip2) } else if name.ends_with(".lz4") { Some(CompressionFormat::Lz4) } else { None }
}

fn compression_format(cli: &CompressArgs) -> fbz::Result<CompressionFormat> {
    let inferred = cli.write.output.as_deref().filter(|path| *path != Path::new("-")).and_then(inferred_compression_format);
    match (cli.format, inferred) {
        (Some(explicit), Some(inferred)) if explicit != inferred => Err(invalid("--format conflicts with the --output filename")),
        (Some(explicit), _) | (None, Some(explicit)) => Ok(explicit),
        (None, None) => Err(invalid("compression format must be selected by --format or the --output filename")),
    }
}

fn compressed_output(input: &Path, format: CompressionFormat, directory: Option<&Path>) -> PathBuf {
    let suffix = match format {
        CompressionFormat::Bzip2 => ".bz2",
        CompressionFormat::Gzip => ".gz",
        CompressionFormat::Lz4 => ".lz4",
        CompressionFormat::TarBzip2 => ".tar.bz2",
        CompressionFormat::TarGzip => ".tar.gz",
        CompressionFormat::TarLz4 => ".tar.lz4",
        CompressionFormat::Zip => ".zip",
    };
    let name = input.file_name().unwrap_or(input.as_os_str());
    let output = PathBuf::from(format!("{}{suffix}", name.to_string_lossy()));
    directory.map_or_else(|| PathBuf::from(format!("{}{suffix}", input.display())), |directory| directory.join(output))
}

fn run_compress(cli: &CompressArgs, common: &Common) -> fbz::Result<()> {
    validate_inputs(&cli.inputs, true)?;
    let format = compression_format(cli)?;
    let archive = matches!(format, CompressionFormat::TarBzip2 | CompressionFormat::TarGzip | CompressionFormat::TarLz4 | CompressionFormat::Zip);
    if cli.filters.is_set() && !archive { return Err(invalid("archive selection options require tar or ZIP compression")); }
    if !archive && cli.write.output.is_some() && cli.inputs.len() != 1 { return Err(invalid("--output requires exactly one input")); }
    if archive && cli.remove_input { return Err(invalid("--rm is not supported when creating archives")); }
    let options = EncodeOptions { threads: common.threads, memory_limit: common.memory_limit, level: cli.level };
    match format {
        CompressionFormat::TarBzip2 | CompressionFormat::TarGzip | CompressionFormat::TarLz4 | CompressionFormat::Zip => compress_archive(cli, format, options, common.quiet),
        CompressionFormat::Bzip2 | CompressionFormat::Gzip | CompressionFormat::Lz4 => compress_streams(cli, format, options, common.quiet),
    }
}

fn archive_output(cli: &CompressArgs, format: CompressionFormat) -> fbz::Result<PathBuf> {
    cli.write.output
        .clone()
        .or_else(|| (cli.inputs.len() == 1 && cli.inputs[0] != "-").then(|| compressed_output(Path::new(&cli.inputs[0]), format, cli.output_dir.as_deref())))
        .ok_or_else(|| invalid("archive compression with multiple inputs requires --output"))
}

fn write_archive_output(cli: &CompressArgs, output: &Path, quiet: bool, write: impl FnOnce(&mut dyn Write) -> fbz::Result<()>) -> fbz::Result<()> {
    if output == Path::new("-") { return write(&mut io::stdout().lock()); }
    if should_skip(output, cli.write.skip_existing, quiet) { return Ok(()); }
    atomic_write(output, cli.write.force, |writer| write(writer))
}

fn compress_archive(cli: &CompressArgs, format: CompressionFormat, options: EncodeOptions, quiet: bool) -> fbz::Result<()> {
    let output = archive_output(cli, format)?;
    let inputs = archive_create::select(&cli.inputs, &cli.filters, &output)?;
    if cli.filters.dry_run { return archive_create::preview(&inputs, &output, quiet); }
    if let Some(directory) = &cli.output_dir { fs::create_dir_all(directory)?; }
    write_archive_output(cli, &output, quiet, |writer| match format {
        CompressionFormat::TarBzip2 => tar_create::pack_bzip2(&inputs, writer, options).map(|_| ()),
        CompressionFormat::TarGzip => tar_create::pack_gzip(&inputs, writer, options).map(|_| ()),
        CompressionFormat::TarLz4 => tar_create::pack_lz4(&inputs, writer, options).map(|_| ()),
        CompressionFormat::Zip => fbz::zip::create_to_writer(&inputs, writer, options).map(|_| ()),
        _ => unreachable!(),
    })
}

fn compress_streams(cli: &CompressArgs, format: CompressionFormat, options: EncodeOptions, quiet: bool) -> fbz::Result<()> {
    if let Some(directory) = &cli.output_dir { fs::create_dir_all(directory)?; }
    for input in &cli.inputs {
        let output = cli.write
            .output
            .clone()
            .unwrap_or_else(|| if input == "-" { PathBuf::from("-") } else { compressed_output(Path::new(input), format, cli.output_dir.as_deref()) });
        if output == Path::new("-") {
            let mut source: Box<dyn Read> = if input == "-" { Box::new(io::stdin().lock()) } else { Box::new(fs::File::open(input)?) };
            let total = if input == "-" { 0 } else { fs::metadata(input)?.len() };
            compress_stream(format, &mut source, &mut io::stdout().lock(), options, input, total, quiet)?;
            if cli.remove_input && input != "-" { fs::remove_file(input)?; }
        } else {
            if should_skip(&output, cli.write.skip_existing, quiet) { continue; }
            let input_path = Path::new(input);
            if input_path == output { return Err(invalid(format!("input and output are both {}", input_path.display()))); }
            let mut source = fs::File::open(input_path)?;
            let total = source.metadata()?.len();
            atomic_write(&output, cli.write.force, |writer| compress_stream(format, &mut source, writer, options, input, total, quiet))?;
            preserve_metadata(input_path, &output)?;
            if cli.remove_input { fs::remove_file(input_path)?; }
        }
    }
    Ok(())
}

fn compress_stream(
    format: CompressionFormat,
    input: &mut impl Read,
    output: &mut impl Write,
    options: EncodeOptions,
    label: &str,
    total: u64,
    quiet: bool,
) -> fbz::Result<()> {
    let format = match format {
        CompressionFormat::Bzip2 => fbz::EncodeFormat::Bzip2,
        CompressionFormat::Gzip => fbz::EncodeFormat::Gzip,
        CompressionFormat::Lz4 => fbz::EncodeFormat::Lz4,
        _ => return Err(invalid("selected format is not a standalone stream codec")),
    };
    let mut display = ProgressDisplay::new(label, total, quiet || total == 0);
    fbz::compress_to_writer_with_progress(input, output, format, options, |progress| display.update_encode(progress)).map(|_| ())
}

fn should_extract(cli: &DecompressArgs, input: &str) -> bool { cli.extract || (cli.write.output.is_none() && is_archive(input)) }

fn run_decode(cli: &DecompressArgs, options: DecodeOptions, quiet: bool) -> fbz::Result<()> {
    validate_inputs(&cli.inputs, true)?;
    if cli.write.output.is_some() && cli.inputs.len() != 1 { return Err(invalid("--output requires exactly one input")); }
    if cli.write.output.is_some() && cli.inputs.iter().any(|input| is_zip_archive(input)) {
        return Err(invalid("--output is not supported for ZIP archives"));
    }
    if cli.write.skip_existing && cli.inputs.iter().any(|input| should_extract(cli, input)) {
        return Err(invalid("--skip-existing is not supported when extracting archives"));
    }
    if let Some(directory) = &cli.output_dir { fs::create_dir_all(directory)?; }
    for input in &cli.inputs {
        let extract = should_extract(cli, input);
        if extract {
            let destination = cli.output_dir.as_deref().unwrap_or_else(|| Path::new("."));
            extract_input(input, destination, cli.write.force, options, cli.max_output, quiet)?;
            if cli.remove_input && input != "-" { fs::remove_file(input)?; }
            continue;
        }
        if input == "-" || cli.write.output.as_deref() == Some(Path::new("-")) {
            decode_input(input, &mut io::stdout().lock(), options, cli.max_output, quiet)?;
            if cli.remove_input && input != "-" { fs::remove_file(input)?; }
            continue;
        }
        let input_path = Path::new(input);
        let output = cli.write.output.clone().unwrap_or_else(|| output_in(input_path, cli.output_dir.as_deref()));
        if input_path == output { return Err(invalid(format!("input and output are both {}", input_path.display()))); }
        if should_skip(&output, cli.write.skip_existing, quiet) { continue; }
        let source = Source::open(input_path)?;
        atomic_write(&output, cli.write.force, |writer| decode_data(source.as_slice(), input, writer, options, cli.max_output, quiet))?;
        preserve_metadata(input_path, &output)?;
        if cli.remove_input { fs::remove_file(input_path)?; }
    }
    Ok(())
}

fn run_index(cli: &IndexArgs, options: DecodeOptions, quiet: bool) -> fbz::Result<()> {
    validate_inputs(&cli.inputs, false)?;
    if cli.write.output.is_some() && cli.inputs.len() != 1 { return Err(invalid("--output requires exactly one input")); }
    for input in &cli.inputs {
        let input_path = Path::new(input);
        let output = cli.write.output.clone().unwrap_or_else(|| PathBuf::from(format!("{}.fbz2i", input_path.display())));
        if output != Path::new("-") && should_skip(&output, cli.write.skip_existing, quiet) { continue; }
        let source = Source::open(input_path)?;
        if select_format(input, source.as_slice())? != Format::Bzip2 { return Err(invalid("index is currently supported only for bzip2 inputs")); }
        let index = build_index_data(source.as_slice(), input, options, cli.max_output, quiet)?;
        let encoded = index.to_bytes();
        if output == Path::new("-") { io::stdout().lock().write_all(&encoded)?; } else { atomic_write(&output, cli.write.force, |writer| writer.write_all(&encoded).map_err(Error::from))?; }
    }
    Ok(())
}

fn run_list(cli: &ListArgs, options: DecodeOptions, quiet: bool) -> fbz::Result<()> {
    validate_inputs(&cli.inputs, false)?;
    let mut values = Vec::new();
    for input in &cli.inputs {
        let source = Source::open(input)?;
        match select_format(input, source.as_slice())? {
            Format::Bzip2 => {
                let index = build_index_data(source.as_slice(), input, options, cli.max_output, quiet)?;
                if cli.json { values.push(index_json(input, &index)); } else { print_index((cli.inputs.len() > 1).then_some(input), &index); }
            }
            Format::Gzip => {
                let report = build_gzip_report_data(source.as_slice(), input, options, cli.max_output, quiet)?;
                if cli.json { values.push(gzip_json(input, &report)); } else { print_gzip_report((cli.inputs.len() > 1).then_some(input), &report); }
            }
            Format::Lz4 => {
                let report = build_lz4_report_data(source.as_slice(), input, options, cli.max_output, quiet)?;
                if cli.json { values.push(lz4_json(input, &report)); } else { print_lz4_report((cli.inputs.len() > 1).then_some(input), &report); }
            }
            Format::Zip => {
                let mut display = ProgressDisplay::new(input, source.as_slice().len() as u64, quiet);
                let report = zip_extract::validate(source.as_slice(), options, cli.max_output, |progress| display.update(progress))?;
                if cli.json { values.push(zip_json(input, &report)); } else { print_zip_report((cli.inputs.len() > 1).then_some(input), &report); }
            }
        }
    }
    if cli.json {
        let value = if values.len() == 1 { values.pop().unwrap() } else { Value::Array(values) };
        serde_json::to_writer_pretty(io::stdout().lock(), &value).map_err(|error| Error::Io(io::Error::other(error)))?;
        println!();
    }
    Ok(())
}

fn extract_input(input: &str, destination: &Path, overwrite: bool, options: DecodeOptions, max_output: Option<usize>, quiet: bool) -> fbz::Result<()> {
    if input == "-" {
        let mut data = Vec::new();
        io::stdin().lock().read_to_end(&mut data)?;
        extract_data(&data, "stdin", destination, overwrite, options, max_output, quiet)
    } else {
        let source = Source::open(input)?;
        extract_data(source.as_slice(), input, destination, overwrite, options, max_output, quiet)
    }
}

fn extract_data(
    data: &[u8],
    label: &str,
    destination: &Path,
    overwrite: bool,
    options: DecodeOptions,
    max_output: Option<usize>,
    quiet: bool,
) -> fbz::Result<()> {
    if select_format(label, data)? == Format::Zip {
        let mut display = ProgressDisplay::new(label, data.len() as u64, quiet);
        zip_extract::unpack(data, destination, overwrite, options, max_output, |progress| display.update(progress)).map(|_| ())
    } else { tar_extract::unpack(destination, overwrite, |writer| decode_data_to_sink(data, label, writer, options, max_output, quiet)) }
}

fn test_input(input: &str, options: DecodeOptions, max_output: Option<usize>, quiet: bool) -> fbz::Result<()> {
    if input == "-" {
        let mut data = Vec::new();
        io::stdin().lock().read_to_end(&mut data)?;
        return test_data(&data, "stdin", options, max_output, quiet);
    }
    let source = Source::open(input)?;
    test_data(source.as_slice(), input, options, max_output, quiet)
}

fn test_data(data: &[u8], label: &str, options: DecodeOptions, max_output: Option<usize>, quiet: bool) -> fbz::Result<()> {
    if select_format(label, data)? == Format::Zip {
        let mut display = ProgressDisplay::new(label, data.len() as u64, quiet);
        zip_extract::validate(data, options, max_output, |progress| display.update(progress)).map(|_| ())
    } else { decode_data(data, label, &mut io::sink(), options, max_output, quiet) }
}

fn decode_input(input: &str, output: &mut impl Write, options: DecodeOptions, max_output: Option<usize>, quiet: bool) -> fbz::Result<()> {
    if input == "-" {
        let mut data = Vec::new();
        io::stdin().lock().read_to_end(&mut data)?;
        decode_data(&data, "stdin", output, options, max_output, quiet)
    } else {
        let source = Source::open(input)?;
        decode_data(source.as_slice(), input, output, options, max_output, quiet)
    }
}

fn decode_data(data: &[u8], label: &str, output: &mut impl Write, options: DecodeOptions, max_output: Option<usize>, quiet: bool) -> fbz::Result<()> {
    let mut output = WriterSink::new(output);
    decode_data_to_sink(data, label, &mut output, options, max_output, quiet)
}

fn decode_data_to_sink(
    data: &[u8],
    label: &str,
    output: &mut impl OutputSink,
    options: DecodeOptions,
    max_output: Option<usize>,
    quiet: bool,
) -> fbz::Result<()> {
    let mut output = LimitedOutput::new(output, max_output);
    let mut display = ProgressDisplay::new(label, data.len() as u64, quiet);
    decode_stream_to_sink_with_progress(select_format(label, data)?, data, &mut output, options, |progress| display.update(progress))
}

fn build_gzip_report_data(data: &[u8], label: &str, options: DecodeOptions, max_output: Option<usize>, quiet: bool) -> fbz::Result<gzip::Report> {
    let mut sink = LimitedOutput::new(io::sink(), max_output);
    let mut display = ProgressDisplay::new(label, data.len() as u64, quiet);
    gzip::decompress_to_writer_with_options_and_progress(data, &mut sink, options, |progress| display.update(progress))
}

fn build_lz4_report_data(data: &[u8], label: &str, options: DecodeOptions, max_output: Option<usize>, quiet: bool) -> fbz::Result<lz4::Report> {
    let mut sink = LimitedOutput::new(io::sink(), max_output);
    let mut display = ProgressDisplay::new(label, data.len() as u64, quiet);
    lz4::decompress_to_writer_with_options_and_progress(data, &mut sink, options, |progress| display.update(progress))
}

fn build_index_data(data: &[u8], label: &str, options: DecodeOptions, max_output: Option<usize>, quiet: bool) -> fbz::Result<Index> {
    let mut display = ProgressDisplay::new(label, data.len() as u64, quiet);
    if let Some(limit) = max_output {
        let mut sink = LimitedOutput::new(io::sink(), Some(limit));
        decode_to_writer_with_progress(data, &mut sink, options, |progress| display.update(progress))
    } else { build_index_with_progress(data, options, |progress| display.update(progress)) }
}

struct LimitedOutput<W> { inner: W, written: usize, limit: Option<usize> }

impl<W> LimitedOutput<W> {
    fn new(inner: W, limit: Option<usize>) -> Self { Self { inner, written: 0, limit } }

    fn check(&self, count: usize) -> io::Result<()> {
        if self.limit.is_some_and(|limit| self.written.saturating_add(count) > limit) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("decoded output exceeds {}", format_bytes(self.limit.unwrap() as u64))));
        }
        Ok(())
    }
}

impl<W: Write> Write for LimitedOutput<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.check(buffer.len())?;
        let written = self.inner.write(buffer)?;
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> { self.inner.flush() }
}
impl<W: OutputSink> OutputSink for LimitedOutput<W> {
    fn write_borrowed(&mut self, buffer: &[u8]) -> io::Result<()> {
        self.check(buffer.len())?;
        self.inner.write_borrowed(buffer)?;
        self.written += buffer.len();
        Ok(())
    }

    fn write_owned_from(&mut self, buffer: Vec<u8>, start: usize) -> io::Result<()> {
        let length = buffer.get(start..).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "owned chunk start exceeds its length"))?.len();
        self.check(length)?;
        self.inner.write_owned_from(buffer, start)?;
        self.written += length;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> { self.inner.flush() }

    fn is_cancelled(&self) -> bool { self.inner.is_cancelled() }
}

struct ProgressDisplay {
    stderr: io::Stderr,
    label: String,
    total: u64,
    started: Instant,
    last_draw: Instant,
    enabled: bool,
    drawn: bool,
}

impl ProgressDisplay {
    fn new(label: &str, total: u64, quiet: bool) -> Self {
        let now = Instant::now();
        Self { stderr: io::stderr(), label: label.into(), total, started: now, last_draw: now, enabled: !quiet && io::stderr().is_terminal(), drawn: false }
    }

    fn update(&mut self, progress: DecodeProgress) {
        let compressed = progress.compressed_bytes.min(self.total);
        let ratio = if compressed == 0 { 0.0 } else { progress.decoded_bytes as f64 / compressed as f64 };
        self.render(compressed, compressed, progress.decoded_bytes, progress.decoded_bytes, ratio);
    }

    fn update_encode(&mut self, progress: EncodeProgress) {
        let input = progress.input_bytes.min(self.total);
        let ratio = if progress.output_bytes == 0 { 0.0 } else { progress.input_bytes as f64 / progress.output_bytes as f64 };
        self.render(input, progress.input_bytes, progress.output_bytes, progress.input_bytes, ratio);
    }

    fn render(&mut self, completed: u64, left: u64, right: u64, throughput: u64, ratio: f64) {
        if !self.enabled { return; }
        let elapsed = self.started.elapsed();
        let finished = completed >= self.total;
        if (!self.drawn && elapsed < Duration::from_millis(200)) || (!finished && self.last_draw.elapsed() < Duration::from_millis(100)) { return; }
        let percent = if self.total == 0 { 100.0 } else { completed as f64 * 100.0 / self.total as f64 };
        let seconds = elapsed.as_secs_f64().max(0.001);
        let rate = throughput as f64 / seconds;
        let eta = if completed == 0 || finished { 0.0 } else { seconds * (self.total - completed) as f64 / completed as f64 };
        let _ = write!(
            self.stderr,
            "\r\x1b[2K{}: {:5.1}%  {} → {}  {}/s  {:.1}×  ETA {}",
            self.label,
            percent,
            format_bytes(left),
            format_bytes(right),
            format_bytes(rate as u64),
            ratio,
            format_duration(eta),
        );
        let _ = self.stderr.flush();
        self.last_draw = Instant::now();
        self.drawn = true;
    }

    fn finish(&mut self) {
        if self.drawn {
            let _ = writeln!(self.stderr);
            self.drawn = false;
        }
    }
}

impl Drop for ProgressDisplay { fn drop(&mut self) { self.finish(); } }

fn should_skip(path: &Path, skip_existing: bool, quiet: bool) -> bool {
    if !path.exists() || !skip_existing { return false; }
    if !quiet { eprintln!("fbz: skipping existing {}", path.display()); }
    true
}

fn preserve_metadata(input: &Path, output: &Path) -> fbz::Result<()> {
    let metadata = fs::metadata(input)?;
    if let Ok(modified) = metadata.modified() { fs::OpenOptions::new().write(true).open(output)?.set_times(fs::FileTimes::new().set_modified(modified))?; }
    fs::set_permissions(output, metadata.permissions())?;
    Ok(())
}

fn atomic_write(path: &Path, force: bool, write: impl FnOnce(&mut fs::File) -> fbz::Result<()>) -> fbz::Result<()> {
    if path.exists() && !force {
        return Err(Error::Io(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists (use --force)", path.display()))));
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    write(temporary.as_file_mut())?;
    temporary.as_file_mut().flush()?;
    if force { temporary.persist(path).map_err(|error| Error::Io(error.error))?; }
    else { temporary.persist_noclobber(path).map_err(|error| Error::Io(error.error))?; }
    Ok(())
}

fn output_in(input: &Path, directory: Option<&Path>) -> PathBuf {
    let output = default_output(input.file_name().map(Path::new).unwrap_or(input));
    directory.map_or_else(|| default_output(input), |directory| directory.join(output))
}

fn format_extension(input: &Path) -> Option<(Format, &'static str)> {
    let extension = input.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "bz2" | "bzip2" => Some((Format::Bzip2, "")),
        "tbz" | "tbz2" => Some((Format::Bzip2, "tar")),
        "gz" | "gzip" => Some((Format::Gzip, "")),
        "tgz" => Some((Format::Gzip, "tar")),
        "lz4" => Some((Format::Lz4, "")),
        "zip" => Some((Format::Zip, "")),
        _ => None,
    }
}

fn is_tar_archive(input: &str) -> bool {
    let input = input.to_ascii_lowercase();
    [".tar.bz2", ".tar.bzip2", ".tbz", ".tbz2", ".tar.gz", ".tar.gzip", ".tgz", ".tar.lz4"].iter().any(|extension| input.ends_with(extension))
}

fn is_zip_archive(input: &str) -> bool { input.to_ascii_lowercase().ends_with(".zip") }

fn is_archive(input: &str) -> bool { is_tar_archive(input) || is_zip_archive(input) }

fn default_output(input: &Path) -> PathBuf {
    format_extension(input).map_or_else(|| PathBuf::from(format!("{}.out", input.display())), |(_, extension)| input.with_extension(extension))
}

fn select_format(input: &str, data: &[u8]) -> fbz::Result<Format> { Format::detect(input, data) }

fn print_index(input: Option<&String>, index: &Index) {
    if let Some(input) = input { println!("input\t{input}"); }
    println!("compressed_bytes\t{}", index.source_len);
    println!("decoded_bytes\t{}", index.decoded_len);
    println!("streams\t{}", index.streams.len());
    println!("blocks\t{}", index.blocks.len());
    for (number, stream) in index.streams.iter().enumerate() {
        println!(
            "stream\t{number}\theader={}\tlevel={}\tblocks={}\tdecoded={}",
            stream.compressed_header_byte, stream.block_size_100k, stream.block_count, stream.decoded_len
        );
    }
}

fn print_gzip_report(input: Option<&String>, report: &gzip::Report) {
    if let Some(input) = input { println!("input\t{input}"); }
    println!("format\tgzip");
    println!("compressed_bytes\t{}", report.source_len);
    println!("decoded_bytes\t{}", report.decoded_len);
    println!("members\t{}", report.members.len());
    println!("blocks\t{}", report.blocks.len());
    println!("speculative_chunks\t{}", report.speculative_chunks);
    println!("fallback_chunks\t{}", report.fallback_chunks);
    for (number, member) in report.members.iter().enumerate() {
        let name = member.name.as_deref().map(String::from_utf8_lossy).unwrap_or_default();
        println!(
            "member\t{number}\tblocks={}\tdecoded={}\tmtime={}\tos={}\tname={name}",
            member_block_count(report, number),
            member.decoded_len,
            member.mtime,
            member.operating_system
        );
    }
}

fn print_lz4_report(input: Option<&String>, report: &lz4::Report) {
    if let Some(input) = input { println!("input\t{input}"); }
    println!("format\tlz4");
    println!("compressed_bytes\t{}", report.source_len);
    println!("decoded_bytes\t{}", report.decoded_len);
    println!("frames\t{}", report.frames.len());
    println!("blocks\t{}", report.blocks.len());
    for (number, frame) in report.frames.iter().enumerate() {
        let mode = match frame.block_mode { lz4::BlockMode::Independent => "independent", lz4::BlockMode::Linked => "linked" };
        println!(
            "frame\t{number}\tmode={mode}\tblock_max={}\tblocks={}\tdecoded={}\tblock_checksums={}\tcontent_checksum={}",
            frame.block_max_size, frame.block_count, frame.decoded_len, frame.block_checksums, frame.content_checksum,
        );
    }
}

fn print_zip_report(input: Option<&String>, report: &zip_extract::Report) {
    if let Some(input) = input { println!("input\t{input}"); }
    println!("format\tzip");
    println!("compressed_bytes\t{}", report.source_len);
    println!("decoded_bytes\t{}", report.decoded_len);
    println!("entries\t{}", report.entries.len());
    for (number, entry) in report.entries.iter().enumerate() {
        println!(
            "entry\t{number}\tmethod={}\tcompressed={}\tdecoded={}\tcrc={:08x}\tpath={}",
            entry.compression_method,
            entry.compressed_size,
            entry.decoded_size,
            entry.crc,
            entry.path.display(),
        );
    }
}

fn member_block_count(report: &gzip::Report, member: usize) -> usize { report.blocks.iter().filter(|block| block.member as usize == member).count() }

fn index_json(input: &str, index: &Index) -> Value {
    json!({
        "input": input,
        "format": "bzip2",
        "source_bytes": index.source_len,
        "source_hash": hex(&index.source_hash),
        "decoded_bytes": index.decoded_len,
        "streams": index.streams.iter().enumerate().map(|(number, stream)| json!({
            "number": number,
            "header_byte": stream.compressed_header_byte,
            "block_size_100k": stream.block_size_100k,
            "first_block": stream.first_block,
            "block_count": stream.block_count,
            "decoded_start": stream.decoded_start,
            "decoded_bytes": stream.decoded_len,
            "eos_bit": stream.eos_bit,
            "expected_crc": stream.expected_stream_crc,
        })).collect::<Vec<_>>(),
        "blocks": index.blocks.iter().enumerate().map(|(number, block)| json!({
            "number": number,
            "compressed_start_bit": block.compressed_start_bit,
            "compressed_end_bit": block.compressed_end_bit,
            "decoded_start": block.decoded_start,
            "decoded_bytes": block.decoded_len,
            "expected_crc": block.expected_crc,
            "stream": block.stream,
        })).collect::<Vec<_>>(),
    })
}

fn gzip_json(input: &str, report: &gzip::Report) -> Value {
    json!({
        "input": input,
        "format": "gzip",
        "source_bytes": report.source_len,
        "decoded_bytes": report.decoded_len,
        "speculative_chunks": report.speculative_chunks,
        "fallback_chunks": report.fallback_chunks,
        "members": report.members.iter().enumerate().map(|(number, member)| json!({
            "number": number,
            "compressed_start": member.compressed_start,
            "deflate_start": member.deflate_start,
            "compressed_end": member.compressed_end,
            "decoded_start": member.decoded_start,
            "decoded_bytes": member.decoded_len,
            "expected_crc": member.expected_crc,
            "mtime": member.mtime,
            "extra_flags": member.extra_flags,
            "operating_system": member.operating_system,
            "name": member.name.as_deref().map(|name| String::from_utf8_lossy(name)),
            "comment": member.comment.as_deref().map(|comment| String::from_utf8_lossy(comment)),
        })).collect::<Vec<_>>(),
        "blocks": report.blocks.iter().enumerate().map(|(number, block)| json!({
            "number": number,
            "member": block.member,
            "kind": block.kind.as_str(),
            "final": block.final_block,
            "compressed_start_bit": block.compressed_start_bit,
            "compressed_end_bit": block.compressed_end_bit,
            "decoded_start": block.decoded_start,
            "decoded_bytes": block.decoded_len,
        })).collect::<Vec<_>>(),
    })
}

fn lz4_json(input: &str, report: &lz4::Report) -> Value {
    json!({
        "input": input,
        "format": "lz4",
        "source_bytes": report.source_len,
        "decoded_bytes": report.decoded_len,
        "frames": report.frames.iter().enumerate().map(|(number, frame)| json!({
            "number": number,
            "compressed_start": frame.compressed_start,
            "compressed_end": frame.compressed_end,
            "decoded_start": frame.decoded_start,
            "decoded_bytes": frame.decoded_len,
            "block_max_size": frame.block_max_size,
            "block_mode": match frame.block_mode { lz4::BlockMode::Independent => "independent", lz4::BlockMode::Linked => "linked" },
            "block_checksums": frame.block_checksums,
            "content_checksum": frame.content_checksum,
            "declared_content_size": frame.declared_content_size,
            "first_block": frame.first_block,
            "block_count": frame.block_count,
        })).collect::<Vec<_>>(),
        "blocks": report.blocks.iter().enumerate().map(|(number, block)| json!({
            "number": number,
            "frame": block.frame,
            "compressed_start": block.compressed_start,
            "compressed_end": block.compressed_end,
            "decoded_start": block.decoded_start,
            "decoded_bytes": block.decoded_len,
            "stored": block.stored,
        })).collect::<Vec<_>>(),
    })
}

fn zip_json(input: &str, report: &zip_extract::Report) -> Value {
    json!({
        "input": input,
        "format": "zip",
        "source_bytes": report.source_len,
        "decoded_bytes": report.decoded_len,
        "entries": report.entries.iter().enumerate().map(|(number, entry)| json!({
            "number": number,
            "path": entry.path,
            "compression_method": entry.compression_method,
            "compressed_bytes": entry.compressed_size,
            "decoded_bytes": entry.decoded_size,
            "expected_crc": entry.crc,
        })).collect::<Vec<_>>(),
    })
}

fn hex(bytes: &[u8]) -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() }

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn format_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    if seconds >= 3600 { format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60) } else if seconds >= 60 { format!("{}m{:02}s", seconds / 60, seconds % 60) } else { format!("{seconds}s") }
}

fn parse_size(value: &str) -> Result<usize, String> {
    let split = value.find(|character: char| !character.is_ascii_digit()).unwrap_or(value.len());
    let number: usize = value[..split].parse().map_err(|_| format!("invalid size {value:?}"))?;
    let multiplier = match value[split..].to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_usize.pow(4),
        _ => return Err(format!("invalid size suffix in {value:?}")),
    };
    number.checked_mul(multiplier).ok_or_else(|| format!("size {value:?} overflows this platform"))
}

fn invalid(message: impl Into<String>) -> Error { Error::InvalidConfiguration(message.into()) }

fn exit_status(error: &Error) -> u8 {
    match error {
        Error::Io(source) if source.kind() == io::ErrorKind::InvalidData => 3,
        Error::Io(_) => 1,
        Error::InvalidConfiguration(_) => 2,
        Error::InvalidStreamHeader
        | Error::InvalidGzip(_)
        | Error::InvalidLz4(_)
        | Error::InvalidZip(_)
        | Error::UnsupportedFormat(_)
        | Error::Decode { .. }
        | Error::InvalidIndex(_) => 3,
        _ => 4,
    }
}
