use anyhow::{anyhow, Context, Result};
use std::io::{self, Read, Seek, SeekFrom};

const MIN_FETCH_SIZE: usize = 1 * 1024 * 1024;
const MAX_FETCH_SIZE: usize = 64 * 1024 * 1024;

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

#[derive(Debug)]
pub struct HttpReader {
    url: String,
    position: u64,
    size: u64,
    fetch_size: usize,
    buffer_start: u64,
    buffer: Vec<u8>,
}

impl HttpReader {
    pub fn new(url: String, chunk_size: u64, parallelism: u32) -> Result<Self> {
        let fetch_size = compute_fetch_size(chunk_size, parallelism);
        let size = probe_size_and_range_support(&url)?;

        Ok(Self {
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

    fn fill_buffer(&mut self) -> io::Result<()> {
        if self.position >= self.size {
            self.buffer.clear();
            self.buffer_start = self.position;
            return Ok(());
        }

        let end_pos = std::cmp::min(
            self.position
                .saturating_add(self.fetch_size.saturating_sub(1) as u64),
            self.size - 1,
        );
        let expected_response_bytes = (end_pos - self.position + 1) as usize;
        let range_header = format!("bytes={}-{}", self.position, end_pos);

        let resp = ureq::get(&self.url)
            .header("Accept-Encoding", "identity")
            .header("Range", &range_header)
            .call()
            .map_err(|e| {
                io::Error::other(format!(
                    "HTTP request failed for range {} at position {}: {}",
                    range_header, self.position, e
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

        let body = read_response_body(resp, expected_response_bytes, &range_header, self.position)?;
        self.buffer_start = self.position;
        self.buffer = body;
        Ok(())
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

fn probe_size_and_range_support(url: &str) -> Result<u64> {
    let range_resp = ureq::get(url)
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

    let head_resp = ureq::head(url)
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
        if buf.is_empty() {
            return Ok(0);
        }
        if self.position >= self.size {
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
            let in_buffer = self.position >= self.buffer_start && self.position < self.buffer_end();
            if !in_buffer {
                self.fill_buffer()?;
                if self.buffer.is_empty() {
                    break;
                }
            }

            let offset = (self.position - self.buffer_start) as usize;
            let available = self.buffer.len().saturating_sub(offset);
            if available == 0 {
                self.buffer.clear();
                continue;
            }

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
            url: self.url.clone(),
            position: self.position,
            size: self.size,
            fetch_size: self.fetch_size,
            buffer_start: self.position,
            buffer: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compute_fetch_size, parse_content_range_total, read_response_body, HttpReader,
        MAX_FETCH_SIZE, MIN_FETCH_SIZE,
    };
    use std::io::{ErrorKind, Seek, SeekFrom};

    #[test]
    fn current_seek_rejects_overflow() {
        let mut reader = HttpReader {
            url: "https://example.invalid/test.gz".into(),
            position: u64::MAX - 1,
            size: u64::MAX - 1,
            fetch_size: MIN_FETCH_SIZE,
            buffer_start: 0,
            buffer: Vec::new(),
        };

        let error = reader.seek(SeekFrom::Current(5)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn end_seek_rejects_overflow() {
        let mut reader = HttpReader {
            url: "https://example.invalid/test.gz".into(),
            position: 0,
            size: u64::MAX - 1,
            fetch_size: MIN_FETCH_SIZE,
            buffer_start: 0,
            buffer: Vec::new(),
        };

        let error = reader.seek(SeekFrom::End(5)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn current_seek_rejects_underflow() {
        let mut reader = HttpReader {
            url: "https://example.invalid/test.gz".into(),
            position: 2,
            size: 10,
            fetch_size: MIN_FETCH_SIZE,
            buffer_start: 0,
            buffer: Vec::new(),
        };

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
}
