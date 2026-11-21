use std::io;
use std::io::SeekFrom;

use memchr::memrchr_iter;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, BufReader,
};

const BLOCK_SIZE: u64 = 1 << 16;
const NEWLINE: u8 = b'\n';

pub async fn tail_lines(
    mut file: impl AsyncRead + AsyncSeek + Unpin,
    mut scan_end: u64,
    max_chars: u64,
    n: usize,
) -> io::Result<Vec<u8>> {
    if n == 0 {
        return Ok(Vec::new());
    }

    let budget_start = scan_end.saturating_sub(max_chars);
    let mut buf = Vec::new();
    let mut counter = 0;
    let mut first_chunk = true;

    let tail_at = loop {
        if scan_end <= budget_start {
            break budget_start;
        }
        let block_size = scan_end.min(BLOCK_SIZE);
        let start = (scan_end - block_size).max(budget_start);
        let len = scan_end - start;
        buf.resize(len as usize, 0);
        file.seek(SeekFrom::Start(start)).await?;
        file.read_exact(&mut buf).await?;

        let mut newlines = memrchr_iter(NEWLINE, &buf);
        if first_chunk && buf.ends_with(&[NEWLINE]) {
            newlines.next();
        }
        first_chunk = false;

        let mut found: Option<u64> = None;
        for offset in newlines {
            if counter + 1 == n {
                found = Some(start + offset as u64 + 1);
                break;
            }
            counter += 1;
        }

        scan_end = start;
        if let Some(at) = found {
            break at;
        }
    };

    file.seek(SeekFrom::Start(tail_at)).await?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).await?;
    Ok(tail)
}

pub async fn head_lines(
    file: impl AsyncRead + AsyncSeek + Unpin,
    max_chars: u64,
    n: usize,
) -> io::Result<Vec<u8>> {
    if n == 0 {
        return Ok(Vec::new());
    }

    let mut lines = BufReader::with_capacity(
        usize::try_from(BLOCK_SIZE).unwrap_or(usize::MAX),
        file.take(max_chars),
    )
    .split(NEWLINE);
    let mut out = Vec::new();
    for _ in 0..n {
        match lines.next_segment().await? {
            Some(line) => {
                out.extend_from_slice(&line);
                out.push(NEWLINE);
            }
            None => break,
        }
    }
    Ok(out)
}
