use std::io::{self, Read, Seek, SeekFrom};
use anyhow::{anyhow, Result};

#[derive(Debug)]
pub struct HttpReader {
    url: String,
    position: u64,
    size: u64,
}

impl HttpReader {
    pub fn new(url: String) -> Result<Self> {
        let resp = ureq::head(&url).call()?;
        
        if resp.status().as_u16() >= 400 {
            return Err(anyhow!("HTTP error: {}", resp.status()));
        }

        let size = resp.headers().get("content-length")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| anyhow!("Server did not return Content-Length, cannot seek/parallelize"))?;

        let range_resp = ureq::get(&url)
            .header("Range", "bytes=0-0")
            .call()?;
        
        if range_resp.status().as_u16() != 206 {
            return Err(anyhow!("Server does not support HTTP Range requests (returned status {}). rapidgzip requires 206 Partial Content for parallel decompression over HTTP.", range_resp.status()));
        }

        Ok(Self {
            url,
            position: 0,
            size,
        })
    }
}

fn checked_relative_seek(base: u64, offset: i64, label: &'static str) -> io::Result<u64> {
    if offset >= 0 {
        base.checked_add(offset as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("Seek overflow for {}", label))
        })
    } else {
        base.checked_sub(offset.unsigned_abs()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("Seek before start of {}", label))
        })
    }
}

impl Read for HttpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.position >= self.size {
            return Ok(0); // EOF
        }

        // Check if interrupted by user
        if crate::is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Interrupted by user"));
        }

        let requested_end = self.position
            .checked_add((buf.len() as u64).saturating_sub(1))
            .unwrap_or(u64::MAX);
        let end_pos = std::cmp::min(requested_end, self.size - 1);
        let range_header = format!("bytes={}-{}", self.position, end_pos);

        let resp = ureq::get(&self.url)
            .header("Range", range_header)
            .call()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("HTTP request failed: {}", e)))?;

        let status = resp.status().as_u16();
        if status != 206 && !(status == 200 && self.position == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected HTTP 206 Partial Content, but got {}. The server might be ignoring Range headers.", status)
            ));
        }

        let mut reader = resp.into_body().into_reader();
        
        let mut total_read = 0;
        while total_read < buf.len() {
            if crate::is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Interrupted by user"));
            }
            let n = reader.read(&mut buf[total_read..])?;
            if n == 0 {
                break;
            }
            total_read += n;
        }
        
        self.position = self.position.checked_add(total_read as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Read position overflow")
        })?;

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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::HttpReader;
    use std::io::{ErrorKind, Seek, SeekFrom};

    #[test]
    fn current_seek_rejects_overflow() {
        let mut reader = HttpReader {
            url: "https://example.invalid/test.gz".into(),
            position: u64::MAX - 1,
            size: u64::MAX - 1,
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
        };

        let error = reader.seek(SeekFrom::Current(-3)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
}
