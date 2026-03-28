use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use rapidgzip::{IoReadMode, Reader, ReaderBuilder};
#[cfg(not(unix))]
use std::io::BufWriter;
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tempfile::{Builder as TempFileBuilder, NamedTempFile};
use url::Url;

mod http_reader;
use http_reader::HttpReader;

// GNU/Linux builds need the native wrapper archives linked explicitly in the
// final binary so the accelerated C++ backend resolves correctly.
#[cfg(target_os = "linux")]
mod native_link {
    #[link(name = "rapidgzip-capi", kind = "static", modifiers = "+whole-archive")]
    unsafe extern "C" {}

    #[link(name = "rpmalloc", kind = "static")]
    unsafe extern "C" {}

    #[link(name = "isal", kind = "static")]
    unsafe extern "C" {}

    #[link(name = "zlibstatic", kind = "static")]
    unsafe extern "C" {}
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum IoReadModeArg {
    /// Keep the native default behavior for the current input type.
    Auto,
    /// Force sequential buffered reads. Useful for HDD-heavy or linear-read workloads.
    Sequential,
    /// Force positioned reads on seekable files. Useful for SSD-friendly parallel access.
    Pread,
    /// Force shared read/seek operations without pread. Useful as a conservative fallback.
    LockedReadAndSeek,
}

impl From<IoReadModeArg> for IoReadMode {
    fn from(value: IoReadModeArg) -> Self {
        match value {
            IoReadModeArg::Auto => IoReadMode::Auto,
            IoReadModeArg::Sequential => IoReadMode::Sequential,
            IoReadModeArg::Pread => IoReadMode::Pread,
            IoReadModeArg::LockedReadAndSeek => IoReadMode::LockedReadAndSeek,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum KeepIndexMode {
    /// Keep the native default behavior.
    Auto,
    /// Keep the in-memory index after reads complete.
    On,
    /// Drop the in-memory index when it is no longer needed.
    Off,
}

static CANCELLED: AtomicBool = AtomicBool::new(false);
const FAST_PATH_READ_SIZE: usize = 256 * 1024 * 1024;
const STREAM_BUFFER_SIZE: usize = 1024 * 1024;

pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = "A high-performance parallel gzip decompressor CLI and benchmarking tool based on rapidgzip-rs."
)]
struct Cli {
    /// Input gzip file, URL (http/https), or '-' for standard input
    #[arg(required = true)]
    input: String,

    /// Output file. If not specified, a local file input is decompressed into an inferred path.
    #[arg(short, long, conflicts_with = "stdout")]
    output: Option<PathBuf>,

    /// Write decompressed data to standard output.
    #[arg(short = 'c', long)]
    stdout: bool,

    /// Force overwriting an existing output file.
    #[arg(short, long)]
    force: bool,

    /// Accepted for gzip compatibility. Only decompression is supported.
    #[arg(short = 'd', long)]
    decompress: bool,

    /// Accepted for gzip compatibility. rapidgzip-rs-cli never deletes the input file.
    #[arg(short, long)]
    keep: bool,

    /// Number of threads (parallelism). 0 means automatic (based on CPU cores).
    #[arg(short = 'P', long, visible_alias = "parallelism", default_value_t = 0)]
    parallelism: u32,

    /// Chunk size in bytes (default: 4194304, i.e., 4MiB)
    #[arg(long, default_value_t = 4194304)]
    chunk_size: u64,

    /// Export index to this file after reading
    #[arg(long)]
    export_index: Option<PathBuf>,

    /// Import index from this file before reading
    #[arg(long)]
    import_index: Option<PathBuf>,

    /// Select the native compressed-input I/O strategy
    #[arg(long, value_enum, default_value_t = IoReadModeArg::Auto)]
    io_read_mode: IoReadModeArg,

    /// Control whether the native reader keeps its in-memory index after use
    #[arg(long, value_enum, default_value_t = KeepIndexMode::Auto)]
    keep_index: KeepIndexMode,

    /// Print the decompressed byte count without writing the data stream
    #[arg(long, conflicts_with_all = ["count_lines", "benchmark_only", "output", "stdout"])]
    count: bool,

    /// Print the number of newline characters in the decompressed stream
    #[arg(short = 'l', long, conflicts_with_all = ["count", "benchmark_only", "output", "stdout"])]
    count_lines: bool,

    /// Discard decompressed data and print benchmark throughput
    #[arg(long, conflicts_with_all = ["count", "count_lines", "output", "stdout"])]
    benchmark_only: bool,

    /// Suppress progress and informational messages.
    #[arg(short, long, conflicts_with = "verbose")]
    quiet: bool,

    /// Print additional progress and diagnostic information.
    #[arg(short, long, conflicts_with = "quiet")]
    verbose: bool,
}

enum InputKind {
    LocalPath(PathBuf),
    Url,
    Stdin,
}

enum OutputTarget {
    Stdout,
    File(PathBuf),
    None,
}

struct PreparedOutputFile {
    final_path: PathBuf,
    temp_file: NamedTempFile,
}

struct InputReader {
    reader: Reader,
    _spooled_input: Option<NamedTempFile>,
}

impl InputReader {
    fn from_reader(reader: Reader) -> Self {
        Self {
            reader,
            _spooled_input: None,
        }
    }

    fn from_spooled_input(reader: Reader, spooled_input: NamedTempFile) -> Self {
        Self {
            reader,
            _spooled_input: Some(spooled_input),
        }
    }
}

impl Read for InputReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Seek for InputReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.reader.seek(pos)
    }
}

fn log_info(cli: &Cli, message: impl AsRef<str>) {
    if !cli.quiet {
        eprintln!("{}", message.as_ref());
    }
}

fn log_verbose(cli: &Cli, message: impl AsRef<str>) {
    if cli.verbose {
        eprintln!("{}", message.as_ref());
    }
}

fn print_summary(total_bytes: u64, duration: std::time::Duration) {
    let speed_mb_s = (total_bytes as f64 / 1024.0 / 1024.0) / duration.as_secs_f64();
    eprintln!("\n=== Benchmark Results ===");
    eprintln!(
        "Uncompressed size : {} bytes ({:.2} MB)",
        total_bytes,
        total_bytes as f64 / 1024.0 / 1024.0
    );
    eprintln!("Time taken        : {:.2?}", duration);
    eprintln!("Throughput        : {:.2} MB/s", speed_mb_s);
}

fn classify_input(input: &str) -> InputKind {
    if input == "-" {
        return InputKind::Stdin;
    }
    if let Ok(url) = Url::parse(input) {
        if url.scheme() == "http" || url.scheme() == "https" {
            return InputKind::Url;
        }
    }
    InputKind::LocalPath(PathBuf::from(input))
}

fn resolved_parallelism(requested_parallelism: u32) -> u32 {
    if requested_parallelism == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
    } else {
        requested_parallelism
    }
}

fn should_force_sequential_for_url_no_index(cli: &Cli, input_kind: &InputKind) -> bool {
    matches!(input_kind, InputKind::Url)
        && cli.import_index.is_none()
        && cli.io_read_mode == IoReadModeArg::Auto
}

fn resolve_io_read_mode(cli: &Cli, input_kind: &InputKind) -> IoReadModeArg {
    if should_force_sequential_for_url_no_index(cli, input_kind) {
        IoReadModeArg::Sequential
    } else {
        cli.io_read_mode
    }
}

fn infer_output_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Could not derive output path from input name. Pass --output <PATH> or --stdout"
            )
        })?;

    let stripped = if let Some(stripped) = file_name.strip_suffix(".gz") {
        stripped
    } else if let Some(stripped) = file_name.strip_suffix(".bgz") {
        stripped
    } else {
        anyhow::bail!(
            "Could not derive output path from input name. Pass --output <PATH> or --stdout"
        );
    };

    if stripped.is_empty() {
        anyhow::bail!(
            "Could not derive output path from input name. Pass --output <PATH> or --stdout"
        );
    }

    Ok(path.with_file_name(stripped))
}

fn resolve_output_target(cli: &Cli, input_kind: &InputKind) -> Result<OutputTarget> {
    if cli.count || cli.count_lines || cli.benchmark_only {
        return Ok(OutputTarget::None);
    }

    if cli.stdout {
        return Ok(OutputTarget::Stdout);
    }

    if let Some(path) = &cli.output {
        return Ok(OutputTarget::File(path.clone()));
    }

    match input_kind {
        InputKind::LocalPath(path) => Ok(OutputTarget::File(infer_output_path(path)?)),
        InputKind::Url | InputKind::Stdin => {
            anyhow::bail!(
                "This input has no safe default output path. Pass --output <PATH> or --stdout"
            );
        }
    }
}

fn spool_stream_to_temp_file<R: Read>(input: &mut R) -> Result<NamedTempFile> {
    let mut temp_file =
        NamedTempFile::new().context("Failed to create temporary file for seekable input")?;
    io::copy(input, temp_file.as_file_mut())
        .context("Failed to spool input into temporary file")?;
    temp_file
        .as_file_mut()
        .flush()
        .context("Failed to flush temporary input file")?;
    temp_file
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .context("Failed to rewind temporary input file")?;
    Ok(temp_file)
}

fn open_seekable_stream<R: Read>(
    input: &mut R,
    builder: &ReaderBuilder,
    label: &str,
) -> Result<InputReader> {
    let temp_file = spool_stream_to_temp_file(input)
        .with_context(|| format!("Failed to spool {} into a temporary file", label))?;
    let reader = builder
        .open(temp_file.path())
        .with_context(|| format!("Failed to open spooled {} in rapidgzip", label))?;
    Ok(InputReader::from_spooled_input(reader, temp_file))
}

fn open_input(cli: &Cli, builder: &ReaderBuilder, input_kind: &InputKind) -> Result<InputReader> {
    match input_kind {
        InputKind::Stdin => {
            log_info(
                cli,
                ">> Spooling standard input into a temporary file for seekable decompression...",
            );
            let stdin = io::stdin();
            let mut stdin_lock = stdin.lock();
            open_seekable_stream(&mut stdin_lock, builder, "standard input")
        }
        InputKind::Url => {
            log_info(cli, format!(">> Opening URL: {}", cli.input));
            let http_reader = HttpReader::new(cli.input.clone(), cli.chunk_size, cli.parallelism)
                .context(
                    "Failed to initialize HTTP range reader. Ensure the server supports Content-Length and Range requests.",
                )?;
            builder
                .open_cloneable_reader(http_reader)
                .map(InputReader::from_reader)
                .context("Failed to open HTTP reader in rapidgzip")
        }
        InputKind::LocalPath(_) => {
            log_info(cli, format!(">> Opening file: {}", cli.input));
            builder
                .open(&cli.input)
                .map(InputReader::from_reader)
                .with_context(|| format!("Failed to open input file: {}", cli.input))
        }
    }
}

fn prepare_output_file(path: &Path, force: bool) -> Result<PreparedOutputFile> {
    if !force && path.exists() {
        anyhow::bail!(
            "Output file already exists: {}. Pass --force to overwrite it",
            path.display()
        );
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp_file = TempFileBuilder::new()
        .prefix(".rapidgzip-rs-cli.")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "Failed to create temporary output file next to {}",
                path.display()
            )
        })?;

    Ok(PreparedOutputFile {
        final_path: path.to_path_buf(),
        temp_file,
    })
}

fn finalize_output_file(mut prepared: PreparedOutputFile, force: bool) -> Result<()> {
    prepared
        .temp_file
        .as_file_mut()
        .sync_all()
        .with_context(|| {
            format!(
                "Failed to flush temporary output file before finalizing {}",
                prepared.final_path.display()
            )
        })?;

    if force && prepared.final_path.exists() {
        std::fs::remove_file(&prepared.final_path).with_context(|| {
            format!(
                "Failed to replace existing output file: {}",
                prepared.final_path.display()
            )
        })?;
    }

    prepared
        .temp_file
        .persist(&prepared.final_path)
        .map_err(|error| {
            anyhow::Error::new(error.error).context(format!(
                "Failed to move temporary output file into place: {}",
                prepared.final_path.display()
            ))
        })?;

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    ctrlc::set_handler(move || {
        eprintln!("\n[!] Ctrl+C received. Cancelling operations...");
        CANCELLED.store(true, Ordering::Relaxed);
    })
    .expect("Error setting Ctrl-C handler");

    let start_time = Instant::now();

    let input_kind = classify_input(&cli.input);
    let effective_io_read_mode = resolve_io_read_mode(&cli, &input_kind);
    let effective_parallelism = resolved_parallelism(cli.parallelism);
    let output_target = resolve_output_target(&cli, &input_kind)?;

    if should_force_sequential_for_url_no_index(&cli, &input_kind) {
        log_info(
            &cli,
            ">> URL input without an imported index uses sequential buffered reads to avoid redundant HTTP range requests.",
        );
    }
    if matches!(input_kind, InputKind::Url)
        && cli.import_index.is_none()
        && effective_io_read_mode == IoReadModeArg::Sequential
        && effective_parallelism > 1
    {
        log_info(
            &cli,
            format!(
                ">> Warning: sequential URL decoding with parallelism {} retains more compressed input in memory for correctness. Memory usage may be significant; prefer -P 1 or import an index if needed.",
                effective_parallelism
            ),
        );
    }

    let mut builder = ReaderBuilder::new()
        .parallelism(cli.parallelism)
        .chunk_size(cli.chunk_size)
        .io_read_mode(effective_io_read_mode.into());

    match cli.keep_index {
        KeepIndexMode::Auto => {
            if cli.import_index.is_some() || cli.export_index.is_some() {
                builder = builder.keep_index(true);
            }
        }
        KeepIndexMode::On => {
            builder = builder.keep_index(true);
        }
        KeepIndexMode::Off => {
            builder = builder.keep_index(false);
        }
    }

    log_verbose(
        &cli,
        format!(
            ">> Builder config: parallelism={}, chunk_size={}, requested_io_read_mode={:?}, effective_io_read_mode={:?}, keep_index={:?}",
            cli.parallelism,
            cli.chunk_size,
            cli.io_read_mode,
            effective_io_read_mode,
            cli.keep_index
        ),
    );

    let mut input = open_input(&cli, &builder, &input_kind)?;

    if let Some(ref index_path) = cli.import_index {
        log_info(&cli, format!(">> Importing index from {:?}", index_path));
        input
            .reader
            .import_index(index_path)
            .with_context(|| "Failed to import index")?;
    }

    if cli.decompress {
        log_verbose(
            &cli,
            ">> --decompress accepted for compatibility; rapidgzip-rs-cli only supports decompression",
        );
    }

    if cli.keep {
        log_verbose(
            &cli,
            ">> --keep accepted for compatibility; rapidgzip-rs-cli never deletes the input file",
        );
    }

    if let OutputTarget::File(path) = &output_target {
        log_verbose(&cli, format!(">> Output file: {}", path.display()));
    }

    let mut prepared_output = match &output_target {
        OutputTarget::File(path) => Some(prepare_output_file(path, cli.force)?),
        OutputTarget::Stdout | OutputTarget::None => None,
    };

    let mut total_bytes = 0u64;

    if cli.count {
        loop {
            if is_cancelled() {
                break;
            }
            let n = input
                .reader
                .read_discard(FAST_PATH_READ_SIZE)
                .with_context(|| "Failed to count decompressed bytes")?;
            if n == 0 {
                break;
            }
            total_bytes += n as u64;
        }
        println!("{}", total_bytes);
    } else if cli.count_lines {
        let mut buffer = vec![0u8; STREAM_BUFFER_SIZE];
        let mut line_count = 0u64;
        loop {
            if is_cancelled() {
                break;
            }
            let n = input
                .read(&mut buffer)
                .with_context(|| "Failed to read decompressed data while counting lines")?;
            if n == 0 {
                break;
            }
            total_bytes += n as u64;
            line_count += bytecount::count(&buffer[..n], b'\n') as u64;
        }
        println!("{}", line_count);
    } else if cli.benchmark_only {
        log_info(
            &cli,
            ">> Decompressing (benchmark-only mode: data will be discarded)...",
        );
        loop {
            if is_cancelled() {
                break;
            }
            let n = input
                .reader
                .read_discard(FAST_PATH_READ_SIZE)
                .with_context(|| "Failed to read compressed data")?;
            if n == 0 {
                break;
            }
            total_bytes += n as u64;
        }
    } else {
        log_info(&cli, ">> Decompressing...");

        #[cfg(unix)]
        {
            match &output_target {
                OutputTarget::File(_) => {
                    let prepared = prepared_output
                        .as_ref()
                        .expect("file output should be prepared before decode");
                    let file_fd = prepared.temp_file.as_file().as_raw_fd();
                    loop {
                        if is_cancelled() {
                            break;
                        }
                        let n = input
                            .reader
                            .read_to_fd(file_fd, FAST_PATH_READ_SIZE)
                            .with_context(|| "Failed to write decompressed data")?;
                        if n == 0 {
                            break;
                        }
                        total_bytes += n as u64;
                    }
                }
                OutputTarget::Stdout => {
                    let stdout = io::stdout();
                    let stdout_lock = stdout.lock();
                    loop {
                        if is_cancelled() {
                            break;
                        }
                        let n = input
                            .reader
                            .read_to_fd(stdout_lock.as_raw_fd(), FAST_PATH_READ_SIZE)
                            .with_context(|| "Failed to write decompressed data")?;
                        if n == 0 {
                            break;
                        }
                        total_bytes += n as u64;
                    }
                }
                OutputTarget::None => unreachable!("output target is validated before decode"),
            }
        }

        #[cfg(not(unix))]
        {
            match &output_target {
                OutputTarget::File(_) => {
                    let prepared = prepared_output
                        .as_mut()
                        .expect("file output should be prepared before decode");
                    let mut out_writer = BufWriter::with_capacity(
                        STREAM_BUFFER_SIZE,
                        prepared.temp_file.as_file_mut(),
                    );
                    let mut buffer = vec![0u8; STREAM_BUFFER_SIZE];
                    loop {
                        if is_cancelled() {
                            break;
                        }
                        let n = input
                            .read(&mut buffer)
                            .with_context(|| "Failed to read compressed data")?;
                        if n == 0 {
                            break;
                        }
                        if let Err(error) = out_writer.write_all(&buffer[..n]) {
                            if is_cancelled() {
                                break;
                            }
                            return Err(error).with_context(|| "Failed to write decompressed data");
                        }
                        total_bytes += n as u64;
                    }
                    if !is_cancelled() {
                        out_writer
                            .flush()
                            .with_context(|| "Failed to flush output buffer")?;
                    }
                }
                OutputTarget::Stdout => {
                    let mut out_writer = BufWriter::with_capacity(STREAM_BUFFER_SIZE, io::stdout());
                    let mut buffer = vec![0u8; STREAM_BUFFER_SIZE];
                    loop {
                        if is_cancelled() {
                            break;
                        }
                        let n = input
                            .read(&mut buffer)
                            .with_context(|| "Failed to read compressed data")?;
                        if n == 0 {
                            break;
                        }
                        if let Err(error) = out_writer.write_all(&buffer[..n]) {
                            if is_cancelled() {
                                break;
                            }
                            return Err(error).with_context(|| "Failed to write decompressed data");
                        }
                        total_bytes += n as u64;
                    }
                    if !is_cancelled() {
                        out_writer
                            .flush()
                            .with_context(|| "Failed to flush output buffer")?;
                    }
                }
                OutputTarget::None => unreachable!("output target is validated before decode"),
            }
        }
    }

    if is_cancelled() {
        log_info(
            &cli,
            ">> Operation cancelled. Temporary output will be discarded.",
        );
        std::process::exit(1);
    }

    if let Some(prepared) = prepared_output.take() {
        finalize_output_file(prepared, cli.force)?;
    }

    let duration = start_time.elapsed();
    if cli.benchmark_only || cli.verbose {
        print_summary(total_bytes, duration);
    }

    if let Some(ref index_path) = cli.export_index {
        log_info(&cli, format!(">> Exporting index to {:?}", index_path));
        input
            .reader
            .export_index(index_path)
            .with_context(|| "Failed to export index")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        classify_input, infer_output_path, open_seekable_stream, resolve_io_read_mode,
        resolve_output_target, should_force_sequential_for_url_no_index, spool_stream_to_temp_file,
        Cli, InputKind, IoReadModeArg, KeepIndexMode, OutputTarget,
    };
    use clap::Parser;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use rapidgzip::ReaderBuilder;
    use std::io::{Cursor, Read, Write};
    use std::path::{Path, PathBuf};

    fn create_test_gz() -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(b"stdin should be spooled to disk before decompression")
            .unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn spool_stream_to_temp_file_copies_all_bytes() {
        let expected = b"abcdefghijklmnopqrstuvwxyz".repeat(1024);
        let mut cursor = Cursor::new(expected.clone());

        let temp_file = spool_stream_to_temp_file(&mut cursor).expect("Failed to spool test input");
        let actual = std::fs::read(temp_file.path()).expect("Failed to read spooled file");

        assert_eq!(actual, expected);
    }

    #[test]
    fn open_seekable_stream_decompresses_spooled_input() {
        let mut cursor = Cursor::new(create_test_gz());
        let builder = ReaderBuilder::new().parallelism(2);
        let mut input = open_seekable_stream(&mut cursor, &builder, "test input")
            .expect("Failed to open spooled test input");

        let mut output = String::new();
        input
            .read_to_string(&mut output)
            .expect("Failed to decompress spooled test input");

        assert_eq!(
            output,
            "stdin should be spooled to disk before decompression"
        );
    }

    #[test]
    fn cli_parses_io_mode_and_keep_index() {
        let cli = Cli::try_parse_from([
            "rapidgzip-rs-cli",
            "input.gz",
            "--io-read-mode",
            "sequential",
            "--keep-index",
            "off",
        ])
        .unwrap();

        assert_eq!(cli.io_read_mode, IoReadModeArg::Sequential);
        assert_eq!(cli.keep_index, KeepIndexMode::Off);
    }

    #[test]
    fn url_without_index_forces_sequential_only_in_auto_mode() {
        let auto_cli =
            Cli::try_parse_from(["rapidgzip-rs-cli", "https://example.org/a.gz"]).unwrap();
        let explicit_cli = Cli::try_parse_from([
            "rapidgzip-rs-cli",
            "https://example.org/a.gz",
            "--io-read-mode",
            "pread",
        ])
        .unwrap();

        assert!(should_force_sequential_for_url_no_index(
            &auto_cli,
            &InputKind::Url
        ));
        assert_eq!(
            resolve_io_read_mode(&auto_cli, &InputKind::Url),
            IoReadModeArg::Sequential
        );
        assert!(!should_force_sequential_for_url_no_index(
            &explicit_cli,
            &InputKind::Url
        ));
        assert_eq!(
            resolve_io_read_mode(&explicit_cli, &InputKind::Url),
            IoReadModeArg::Pread
        );
    }

    #[test]
    fn cli_accepts_compatibility_flags() {
        let cli = Cli::try_parse_from([
            "rapidgzip-rs-cli",
            "input.gz",
            "--decompress",
            "--keep",
            "--stdout",
            "--force",
        ])
        .unwrap();

        assert!(cli.decompress);
        assert!(cli.keep);
        assert!(cli.stdout);
        assert!(cli.force);
    }

    #[test]
    fn cli_parses_count_modes() {
        let cli =
            Cli::try_parse_from(["rapidgzip-rs-cli", "input.gz", "--count-lines", "--verbose"])
                .unwrap();

        assert!(cli.count_lines);
        assert!(cli.verbose);
    }

    #[test]
    fn classify_input_recognizes_local_url_and_stdin() {
        assert!(matches!(classify_input("-"), InputKind::Stdin));
        assert!(matches!(
            classify_input("https://example.org/a.gz"),
            InputKind::Url
        ));
        assert!(matches!(classify_input("file.gz"), InputKind::LocalPath(_)));
    }

    #[test]
    fn infer_output_path_strips_gzip_extensions() {
        assert_eq!(
            infer_output_path(Path::new("reads.fastq.gz")).unwrap(),
            PathBuf::from("reads.fastq")
        );
        assert_eq!(
            infer_output_path(Path::new("reads.fastq.bgz")).unwrap(),
            PathBuf::from("reads.fastq")
        );
        assert!(infer_output_path(Path::new("reads.fastq")).is_err());
    }

    #[test]
    fn resolve_output_target_requires_explicit_output_for_stdin_and_url() {
        let stdin_cli = Cli::try_parse_from(["rapidgzip-rs-cli", "-"]).unwrap();
        assert!(resolve_output_target(&stdin_cli, &InputKind::Stdin).is_err());

        let url_cli =
            Cli::try_parse_from(["rapidgzip-rs-cli", "https://example.org/a.gz"]).unwrap();
        assert!(resolve_output_target(&url_cli, &InputKind::Url).is_err());

        let file_cli = Cli::try_parse_from(["rapidgzip-rs-cli", "reads.fastq.gz"]).unwrap();
        match resolve_output_target(
            &file_cli,
            &InputKind::LocalPath(PathBuf::from("reads.fastq.gz")),
        )
        .unwrap()
        {
            OutputTarget::File(path) => assert_eq!(path, PathBuf::from("reads.fastq")),
            _ => panic!("expected file output target"),
        }
    }
}
