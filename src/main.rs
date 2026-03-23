use anyhow::{Context, Result};
use clap::Parser;
use rapidgzip::{Reader, ReaderBuilder};
use std::fs::File;
#[cfg(not(unix))]
use std::io::BufWriter;
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tempfile::NamedTempFile;
use url::Url;

mod http_reader;
use http_reader::HttpReader;

// GNU/Linux builds need the native wrapper archives linked explicitly in the
// final binary so the accelerated C++ backend resolves correctly.
#[cfg(not(target_env = "msvc"))]
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

static CANCELLED: AtomicBool = AtomicBool::new(false);
const FAST_PATH_READ_SIZE: usize = 256 * 1024 * 1024;

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

    /// Output file. If not specified, outputs to stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Number of threads (parallelism). 0 means automatic (based on CPU cores).
    #[arg(short, long, default_value_t = 0)]
    parallelism: u32,

    /// Chunk size in bytes (default: 4194304, i.e., 4MiB)
    #[arg(short, long, default_value_t = 4194304)]
    chunk_size: u64,

    /// Export index to this file after reading
    #[arg(long)]
    export_index: Option<PathBuf>,

    /// Import index from this file before reading
    #[arg(long)]
    import_index: Option<PathBuf>,

    /// Suppress output/decompression data and only show benchmarks (useful for speed tests)
    #[arg(short, long, default_value_t = false)]
    quiet: bool,
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

fn open_input(cli: &Cli, builder: &ReaderBuilder) -> Result<InputReader> {
    if cli.input == "-" {
        eprintln!(">> Spooling standard input into a temporary file for seekable decompression...");
        let stdin = io::stdin();
        let mut stdin_lock = stdin.lock();
        return open_seekable_stream(&mut stdin_lock, builder, "standard input");
    }

    if let Ok(url) = Url::parse(&cli.input) {
        if url.scheme() == "http" || url.scheme() == "https" {
            eprintln!(">> Opening URL: {}", cli.input);
            let http_reader = HttpReader::new(cli.input.clone())
                .context("Failed to initialize HTTP range reader. Ensure the server supports Range requests.")?;
            return builder
                .open_cloneable_reader(http_reader)
                .map(InputReader::from_reader)
                .context("Failed to open HTTP reader in rapidgzip");
        }
    }

    eprintln!(">> Opening file: {}", cli.input);
    builder
        .open(&cli.input)
        .map(InputReader::from_reader)
        .with_context(|| format!("Failed to open input file: {}", cli.input))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    ctrlc::set_handler(move || {
        eprintln!(
            "
[!] Ctrl+C received. Cancelling operations..."
        );
        CANCELLED.store(true, Ordering::Relaxed);
    })
    .expect("Error setting Ctrl-C handler");

    let start_time = Instant::now();

    let keep_index = cli.import_index.is_some() || cli.export_index.is_some();
    let builder = ReaderBuilder::new()
        .parallelism(cli.parallelism)
        .chunk_size(cli.chunk_size)
        .keep_index(keep_index);

    let mut input = open_input(&cli, &builder)?;

    if let Some(ref index_path) = cli.import_index {
        eprintln!(">> Importing index from {:?}", index_path);
        input
            .reader
            .import_index(index_path)
            .with_context(|| "Failed to import index")?;
    }

    let mut total_bytes = 0;

    if cli.quiet {
        eprintln!(">> Decompressing (quiet mode: data will be discarded)...");
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
        eprintln!(">> Decompressing...");

        #[cfg(unix)]
        {
            match cli.output {
                Some(ref path) => {
                    let file =
                        File::create(path).with_context(|| "Failed to create output file")?;
                    loop {
                        if is_cancelled() {
                            break;
                        }
                        let n = input
                            .reader
                            .read_to_fd(file.as_raw_fd(), FAST_PATH_READ_SIZE)
                            .with_context(|| "Failed to write decompressed data")?;
                        if n == 0 {
                            break;
                        }
                        total_bytes += n as u64;
                    }
                }
                None => {
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
            }
        }

        #[cfg(not(unix))]
        {
            let mut out_writer: Box<dyn Write> = match cli.output {
                Some(ref path) => {
                    let file =
                        File::create(path).with_context(|| "Failed to create output file")?;
                    Box::new(BufWriter::with_capacity(1024 * 1024, file))
                }
                None => Box::new(io::stdout()),
            };

            let mut buffer = vec![0u8; 1024 * 1024];
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
    }

    if is_cancelled() {
        eprintln!(">> Operation cancelled. Incomplete output may have been written.");
        if let Some(ref path) = cli.output {
            eprintln!(">> Removing incomplete output file: {:?}", path);
            let _ = std::fs::remove_file(path);
        }
        std::process::exit(1);
    }

    let duration = start_time.elapsed();
    let speed_mb_s = (total_bytes as f64 / 1024.0 / 1024.0) / duration.as_secs_f64();

    eprintln!(
        "
=== Benchmark Results ==="
    );
    eprintln!(
        "Uncompressed size : {} bytes ({:.2} MB)",
        total_bytes,
        total_bytes as f64 / 1024.0 / 1024.0
    );
    eprintln!("Time taken        : {:.2?}", duration);
    eprintln!("Throughput        : {:.2} MB/s", speed_mb_s);

    if let Some(ref index_path) = cli.export_index {
        eprintln!(">> Exporting index to {:?}", index_path);
        input
            .reader
            .export_index(index_path)
            .with_context(|| "Failed to export index")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{open_seekable_stream, spool_stream_to_temp_file};
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use rapidgzip::ReaderBuilder;
    use std::io::{Cursor, Read, Write};

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
}
