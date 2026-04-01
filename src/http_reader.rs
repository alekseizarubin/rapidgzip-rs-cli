use anyhow::{anyhow, Context, Result};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

const MIN_FETCH_SIZE: usize = 1024 * 1024;
const MAX_FETCH_SIZE: usize = 64 * 1024 * 1024;

/// How many fetch-chunks to keep *behind* the current read position.
/// Backward seeks within this window are served from the in-memory cache
/// without issuing a new HTTP request.
const KEEP_BEHIND_FETCH_BLOCKS: usize = 2;

/// Total sliding-window size in fetch-chunks (keep-behind + look-ahead).
/// Once the buffer exceeds this, old data is evicted from the front —
/// but only up to `KEEP_BEHIND_FETCH_BLOCKS` behind the current position.
const MAX_WINDOW_FETCH_BLOCKS: usize = KEEP_BEHIND_FETCH_BLOCKS + 2;

/// Hard cap on the sliding window regardless of fetch_size.
/// Prevents excessive memory use when fetch_size is large.
const MAX_WINDOW_BYTES: usize = 128 * 1024 * 1024;

fn read_response_body(
    response: ureq::http::Response<ureq::Body>,
    expected_response_bytes: usize,
    range_header: &str,
    position: u64,
) -> io::Result<Vec<u8>> {
    let body_limit = expected_response_bytes.saturating_add(1) as u64;
    let body = response
        .into_body()
        .into_with_config()
        // ureq checks the limit before the next read, so exact-size bodies need
        // one extra byte of headroom for the final EOF read.
        .limit(body_limit)
        .read_to_vec()
        .map_err(|e| {
            io::Error::other(format!(
                "Failed to read HTTP response body for range {} at position {}: {}",
                range_header, position, e
            ))
        })?;
    if body.len() != expected_response_bytes {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "HTTP body length mismatch for range {}: expected {} bytes, got {}",
                range_header,
                expected_response_bytes,
                body.len()
            ),
        ));
    }
    Ok(body)
}

/// HTTP range reader with a sliding in-memory cache.
///
/// The buffer is a contiguous window `[buffer_start, buffer_start + buffer.len())`.
/// Forward reads append to the buffer; the front is evicted only when the window
/// exceeds `MAX_WINDOW_FETCH_BLOCKS * fetch_size` (capped at `MAX_WINDOW_BYTES`),
/// always preserving at least `KEEP_BEHIND_FETCH_BLOCKS * fetch_size` bytes
/// behind the current position so that small backward seeks are served from
/// cache without a new HTTP request.
///
/// A backward seek that falls outside the retained window forces a full reset
/// and a fresh fetch from the new position.
#[derive(Debug)]
pub struct HttpReader {
    // Shared across all clones so each parallel worker reuses the same
    // connection pool instead of establishing a fresh TCP connection per fetch.
    agent: Arc<ureq::Agent>,
    url: String,
    position: u64,
    size: u64,
    fetch_size: usize,
    /// Byte offset of `buffer[0]` in the remote file.
    buffer_start: u64,
    /// Cached bytes `[buffer_start, buffer_start + buffer.len())`.
    buffer: Vec<u8>,
}

impl HttpReader {
    pub fn new(url: String, chunk_size: u64, parallelism: u32) -> Result<Self> {
        let fetch_size = compute_fetch_size(chunk_size, parallelism);
        let agent = Arc::new(ureq::Agent::new_with_defaults());
        let size = probe_size_and_range_support(&agent, &url)?;

        Ok(Self {
            agent,
            url,
            position: 0,
            size,
            fetch_size,
            buffer_start: 0,
            buffer: Vec::new(),
        })
    }

    fn buffer_end(&self) -> u64 {
        self.buffer_start + self.buffer.len() as u64
    }

    /// Ensures `self.position` is covered by the buffer.
    ///
    /// - If position is already in the buffer: no-op (cache hit).
    /// - If position == buffer_end(): contiguous forward read — append the next chunk.
    /// - Any other miss (forward gap, backward past window): reset the buffer to
    ///   `position` and fetch from there. Fetching from `buffer_end()` for a gap
    ///   would leave `position` outside the buffer and cause a false `Ok(0)` EOF.
    fn ensure_buffered(&mut self) -> io::Result<()> {
        if self.position >= self.size {
            return Ok(());
        }
        // Cache hit.
        if self.position >= self.buffer_start && self.position < self.buffer_end() {
            return Ok(());
        }

        // Contiguous forward read: keep the existing buffer and append.
        // Gap or backward-past-window: reset so the next fetch starts at position.
        if self.buffer.is_empty() || self.position != self.buffer_end() {
            self.buffer.clear();
            self.buffer_start = self.position;
        }

        self.fetch_and_append(self.buffer_end())?;
        self.maybe_evict();
        Ok(())
    }

    /// Fetches one HTTP chunk starting at `from` and appends it to the buffer.
    fn fetch_and_append(&mut self, from: u64) -> io::Result<()> {
        if from >= self.size {
            return Ok(());
        }
        let end_pos = (from + self.fetch_size as u64 - 1).min(self.size - 1);
        let expected_response_bytes = (end_pos - from + 1) as usize;
        let range_header = format!("bytes={}-{}", from, end_pos);

        let resp = self
            .agent
            .get(&self.url)
            .header("Accept-Encoding", "identity")
            .header("Range", &range_header)
            .call()
            .map_err(|e| {
                io::Error::other(format!(
                    "HTTP request failed for range {} at position {}: {}",
                    range_header, from, e
                ))
            })?;

        let status = resp.status().as_u16();
        if status != 206 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Expected HTTP 206 Partial Content, but got {}. The server might be ignoring Range headers.",
                    status
                ),
            ));
        }

        let body = read_response_body(resp, expected_response_bytes, &range_header, from)?;
        self.buffer.extend_from_slice(&body);
        Ok(())
    }

    /// Evicts data from the front of the buffer when it exceeds the window limit,
    /// keeping at least `KEEP_BEHIND_FETCH_BLOCKS * fetch_size` bytes behind
    /// the current position as a source of truth for backward seeks.
    fn maybe_evict(&mut self) {
        let max_window =
            (self.fetch_size.saturating_mul(MAX_WINDOW_FETCH_BLOCKS)).min(MAX_WINDOW_BYTES);
        if self.buffer.len() <= max_window {
            return;
        }
        let keep_behind =
            (self.fetch_size.saturating_mul(KEEP_BEHIND_FETCH_BLOCKS)).min(MAX_WINDOW_BYTES / 2);
        let current_offset = self.position.saturating_sub(self.buffer_start) as usize;
        let evictable = current_offset.saturating_sub(keep_behind);
        if evictable == 0 {
            return;
        }
        self.buffer.drain(..evictable);
        self.buffer_start += evictable as u64;
    }
}

fn parse_content_length<B>(resp: &ureq::http::Response<B>) -> Option<u64> {
    resp.headers()
        .get("content-length")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

fn parse_content_range_total<B>(resp: &ureq::http::Response<B>) -> Option<u64> {
    let header = resp.headers().get("content-range")?.to_str().ok()?;
    let (_, total) = header.rsplit_once('/')?;
    total.parse::<u64>().ok()
}

fn probe_size_and_range_support(agent: &ureq::Agent, url: &str) -> Result<u64> {
    let range_resp = agent
        .get(url)
        .header("Accept-Encoding", "identity")
        .header("Range", "bytes=0-0")
        .call()
        .context("Failed to probe HTTP range support")?;

    if range_resp.status().as_u16() == 206 {
        if let Some(size) = parse_content_range_total(&range_resp) {
            return Ok(size);
        }
        return Err(anyhow!(
            "Server returned HTTP 206 but did not provide a valid Content-Range header"
        ));
    }

    let head_resp = agent
        .head(url)
        .header("Accept-Encoding", "identity")
        .call()
        .context("Failed to probe HTTP metadata with HEAD after range GET failed")?;

    if head_resp.status().as_u16() >= 400 {
        return Err(anyhow!("HTTP error: {}", head_resp.status()));
    }

    let size = parse_content_length(&head_resp)
        .ok_or_else(|| anyhow!("Server did not return Content-Length, cannot seek/parallelize"))?;

    Err(anyhow!(
        "Server does not support HTTP Range requests (returned status {} to a range GET). rapidgzip requires 206 Partial Content for parallel decompression over HTTP. Reported Content-Length was {} bytes.",
        range_resp.status(),
        size
    ))
}

fn compute_fetch_size(chunk_size: u64, parallelism: u32) -> usize {
    let effective_parallelism = if parallelism == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        parallelism as usize
    };

    let target = (chunk_size as u128)
        .saturating_mul(effective_parallelism as u128)
        .saturating_mul(2);

    let capped = target.min(MAX_FETCH_SIZE as u128) as usize;
    capped.max(MIN_FETCH_SIZE)
}

fn checked_relative_seek(base: u64, offset: i64, label: &'static str) -> io::Result<u64> {
    if offset >= 0 {
        base.checked_add(offset as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Seek overflow for {}", label),
            )
        })
    } else {
        base.checked_sub(offset.unsigned_abs()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Seek before start of {}", label),
            )
        })
    }
}

impl Read for HttpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.position >= self.size {
            return Ok(0);
        }
        if crate::is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Interrupted by user",
            ));
        }

        let mut total_read = 0;
        while total_read < buf.len() && self.position < self.size {
            self.ensure_buffered()?;

            // After ensure_buffered the position must be inside the buffer
            // unless we hit EOF.
            if self.position < self.buffer_start || self.position >= self.buffer_end() {
                break;
            }

            let offset = (self.position - self.buffer_start) as usize;
            let available = self.buffer.len() - offset;
            let to_copy = available.min(buf.len() - total_read);
            buf[total_read..total_read + to_copy]
                .copy_from_slice(&self.buffer[offset..offset + to_copy]);
            total_read += to_copy;
            self.position = self.position.checked_add(to_copy as u64).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Read position overflow")
            })?;
        }

        Ok(total_read)
    }
}

impl Seek for HttpReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(p) => checked_relative_seek(self.position, p, "stream")?,
            SeekFrom::End(p) => checked_relative_seek(self.size, p, "stream")?,
        };

        self.position = new_pos;
        Ok(self.position)
    }
}

impl rapidgzip::CloneableReadSeek for HttpReader {
    fn clone_box(&self) -> Box<dyn rapidgzip::CloneableReadSeek> {
        Box::new(HttpReader {
            // Share the connection pool — no new TCP handshake per parallel worker.
            agent: Arc::clone(&self.agent),
            url: self.url.clone(),
            position: self.position,
            size: self.size,
            fetch_size: self.fetch_size,
            // Each clone starts with an empty buffer at the current position.
            // The C++ backend always seeks each worker to its target offset
            // before reading, so the starting buffer state doesn't matter.
            buffer_start: self.position,
            buffer: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compute_fetch_size, parse_content_range_total, read_response_body, HttpReader,
        KEEP_BEHIND_FETCH_BLOCKS, MAX_FETCH_SIZE, MAX_WINDOW_FETCH_BLOCKS, MIN_FETCH_SIZE,
    };
    use std::io::{ErrorKind, Read, Seek, SeekFrom};
    use std::sync::Arc;

    fn test_reader(position: u64, size: u64) -> HttpReader {
        HttpReader {
            agent: Arc::new(ureq::Agent::new_with_defaults()),
            url: "https://example.invalid/test.gz".into(),
            position,
            size,
            fetch_size: MIN_FETCH_SIZE,
            buffer_start: 0,
            buffer: Vec::new(),
        }
    }

    fn reader_with_buffer(position: u64, buffer_start: u64, buffer: Vec<u8>) -> HttpReader {
        let size = buffer_start + buffer.len() as u64 + 1024;
        HttpReader {
            agent: Arc::new(ureq::Agent::new_with_defaults()),
            url: "https://example.invalid/test.gz".into(),
            position,
            size,
            fetch_size: MIN_FETCH_SIZE,
            buffer_start,
            buffer,
        }
    }

    #[test]
    fn current_seek_rejects_overflow() {
        let mut reader = test_reader(u64::MAX - 1, u64::MAX - 1);
        let error = reader.seek(SeekFrom::Current(5)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn end_seek_rejects_overflow() {
        let mut reader = test_reader(0, u64::MAX - 1);
        let error = reader.seek(SeekFrom::End(5)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn current_seek_rejects_underflow() {
        let mut reader = test_reader(2, 10);
        let error = reader.seek(SeekFrom::Current(-3)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn fetch_size_has_minimum_and_cap() {
        assert_eq!(compute_fetch_size(1, 1), MIN_FETCH_SIZE);
        assert_eq!(compute_fetch_size(u64::MAX, u32::MAX), MAX_FETCH_SIZE);
    }

    #[test]
    fn fetch_size_uses_chunk_size_and_parallelism() {
        assert_eq!(compute_fetch_size(4 * 1024 * 1024, 2), 16 * 1024 * 1024);
    }

    #[test]
    fn parse_total_size_from_content_range() {
        let resp = ureq::http::Response::builder()
            .status(206)
            .header("Content-Range", "bytes 0-0/12345")
            .body(())
            .unwrap();
        assert_eq!(parse_content_range_total(&resp), Some(12345));
    }

    #[test]
    fn large_http_range_reads_are_not_limited_by_ureq_default_cap() {
        let data = (0..(11 * 1024 * 1024 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let response = ureq::http::Response::builder()
            .status(206)
            .header("Content-Length", data.len().to_string())
            .body(ureq::Body::builder().data(data.clone()))
            .unwrap();

        let body = read_response_body(response, data.len(), "bytes=0-11534352", 0).unwrap();

        assert_eq!(body, data);
    }

    /// Backward seek within the retained window must be a cache hit —
    /// position is updated, but ensure_buffered must report the position
    /// as already in the buffer without fetching.
    #[test]
    fn backward_seek_within_window_is_cache_hit() {
        // Fill a buffer: bytes 0..4096, position at 4096 (just past end).
        let buf_data: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let mut reader = reader_with_buffer(4096, 0, buf_data.clone());

        // Seek backward to byte 1024 — still within buffer.
        reader.seek(SeekFrom::Start(1024)).unwrap();

        // Read 16 bytes — must come from cache (no HTTP possible in unit test).
        let mut out = vec![0u8; 16];
        let n = reader.read(&mut out).unwrap();
        assert_eq!(n, 16);
        assert_eq!(out, buf_data[1024..1040]);
    }

    /// maybe_evict must not evict data within KEEP_BEHIND_FETCH_BLOCKS
    /// of the current position.
    #[test]
    fn eviction_keeps_required_tail() {
        let fetch_size = MIN_FETCH_SIZE;
        // Build a buffer large enough to trigger eviction:
        // MAX_WINDOW_FETCH_BLOCKS + 1 fetch blocks.
        let window_bytes = fetch_size * (MAX_WINDOW_FETCH_BLOCKS + 1);
        let buf_data: Vec<u8> = vec![0xAB; window_bytes];
        // Position is at the very end of the buffer.
        let position = window_bytes as u64;
        let mut reader = reader_with_buffer(position, 0, buf_data);

        reader.maybe_evict();

        // After eviction, buffer_start must still be within KEEP_BEHIND_FETCH_BLOCKS
        // of position (i.e., at most keep_behind bytes before position).
        let keep_behind = (fetch_size * KEEP_BEHIND_FETCH_BLOCKS) as u64;
        let min_start = position.saturating_sub(keep_behind);
        assert!(
            reader.buffer_start >= min_start,
            "buffer_start ({}) must be >= {} (position {} - keep_behind {})",
            reader.buffer_start,
            min_start,
            position,
            keep_behind
        );
        // The position itself must still be in the buffer.
        assert!(
            reader.buffer_start <= position && position <= reader.buffer_end(),
            "position must still be within buffer after eviction"
        );
    }

    /// A forward seek with a gap (position > buffer_end) must reset the buffer
    /// to `position` before fetching, not append from `buffer_end()`.
    ///
    /// Old bug: `ensure_buffered` fetched from `buffer_end()` (= 1024), leaving
    /// `position` (= 99_999) outside the updated buffer, so `read()` hit the
    /// "position not in buffer" guard and returned `Ok(0)` — a false EOF.
    ///
    /// After the fix the reset path is taken and a fetch from 99_999 is attempted.
    /// That fetch fails (no network in unit tests), which is the correct outcome:
    /// an `Err` rather than a silent `Ok(0)`.
    #[test]
    fn forward_seek_gap_does_not_return_false_eof() {
        let buf_data: Vec<u8> = vec![0u8; 1024];
        // buffer covers [0, 1024); jump far past it.
        let far_position = 99_999u64;
        // size must be larger than far_position so the early-EOF guard in read()
        // is not triggered before ensure_buffered is even called.
        let size = far_position + 1024;
        let mut reader = HttpReader {
            agent: Arc::new(ureq::Agent::new_with_defaults()),
            url: "https://example.invalid/test.gz".into(),
            position: far_position,
            size,
            fetch_size: MIN_FETCH_SIZE,
            buffer_start: 0,
            buffer: buf_data,
        };

        let mut out = vec![0u8; 16];
        let result = reader.read(&mut out);
        // Must NOT be Ok(0): that would be a false EOF before the file ends.
        // Must be Err (network failure for the required fetch).
        assert!(
            result.is_err(),
            "expected a network error from the required fetch, not Ok(0) (false EOF); got {:?}",
            result
        );
    }
}
