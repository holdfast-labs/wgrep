//! Bounded socket frame reader: buffered, whole-frame deadline.
//!
//! Replaces one-byte-per-`read` loops (up to 65k syscalls per 64 KiB
//! frame; ~20k syscalls for a 20 KiB reply — material client latency).
//! Semantics preserved exactly:
//! - `\n`-terminated; the terminating newline may exceed the cap by 1,
//!   exactly as the previous byte loop behaved (it broke before checking);
//! - anything longer, or a close before newline, is an error;
//! - UTF-8 is checked by the callers, not here (client parses bytes).
//!
//! The WHOLE call is bounded by `whole` (the documented 120 s resource
//! contract), not just each `recv`: the socket timeout is recomputed per
//! 8 KiB fill as `min(30 s, remaining)`, so a 1-byte trickle cannot
//! stretch a frame past the budget. Used by the daemon request path and
//! the client reply path alike.

use std::io::BufRead;
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Buffered fill size: a max-size frame costs ~8 syscalls, not ~65k.
const FILL_BUF: usize = 8192;
/// Per-fill ceiling inside the whole-frame budget.
const PER_FILL: Duration = Duration::from_secs(30);

pub(crate) fn read_capped_frame(
    stream: &TcpStream,
    cap: usize,
    whole: Duration,
) -> anyhow::Result<Vec<u8>> {
    let end = Instant::now() + whole;
    let mut reader = std::io::BufReader::with_capacity(FILL_BUF, stream);
    let mut out = Vec::with_capacity(1024.min(cap));
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("wgrep-sem: frame deadline exceeded");
        }
        stream.set_read_timeout(Some(remaining.min(PER_FILL)))?;
        let n = {
            let chunk = reader
                .fill_buf()
                .map_err(|e| anyhow::anyhow!("wgrep-sem: read failed: {e}"))?;
            if chunk.is_empty() {
                anyhow::bail!("wgrep-sem: connection closed mid-frame");
            }
            match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    // Newline included; may exceed the cap by exactly 1
                    // (preserved off-by-one, see module docs).
                    if out.len() + i + 1 > cap + 1 {
                        anyhow::bail!("wgrep-sem: frame exceeds {cap} bytes");
                    }
                    out.extend_from_slice(&chunk[..=i]);
                    let n = i + 1;
                    reader.consume(n);
                    return Ok(out);
                }
                None => {
                    if out.len() + chunk.len() > cap {
                        anyhow::bail!("wgrep-sem: frame exceeds {cap} bytes");
                    }
                    out.extend_from_slice(chunk);
                    chunk.len()
                }
            }
        };
        reader.consume(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let tx = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (rx, _) = l.accept().unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        (tx, rx)
    }

    #[test]
    fn exact_frame_round_trips() {
        let (mut tx, rx) = pair();
        tx.write_all(b"{\"ping\":true}\n").unwrap();
        let got = read_capped_frame(&rx, 64 << 10, Duration::from_secs(10)).unwrap();
        assert_eq!(got, b"{\"ping\":true}\n");
    }

    #[test]
    fn over_cap_rejected() {
        let (mut tx, rx) = pair();
        tx.write_all(&vec![b'x'; 100]).unwrap();
        tx.write_all(b"\n").unwrap();
        let err = read_capped_frame(&rx, 10, Duration::from_secs(10)).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn close_before_newline_errors() {
        let (tx, rx) = pair();
        drop(tx);
        let err = read_capped_frame(&rx, 64 << 10, Duration::from_secs(10)).unwrap_err();
        assert!(err.to_string().contains("closed"));
    }

    #[test]
    fn trickle_within_budget_succeeds() {
        // 1 byte per 50 ms × 5 bytes + newline, whole budget 5 s: fine.
        let (mut tx, rx) = pair();
        std::thread::spawn(move || {
            for _ in 0..5 {
                tx.write_all(b"a").unwrap();
                std::thread::sleep(Duration::from_millis(50));
            }
            tx.write_all(b"\n").unwrap();
        });
        let got = read_capped_frame(&rx, 64 << 10, Duration::from_secs(5)).unwrap();
        assert_eq!(got, b"aaaaa\n");
    }

    #[test]
    fn trickle_past_budget_fails() {
        // 1 byte per 150 ms with a 200 ms whole budget: must fail, never
        // hang (the old per-recv timeout would have allowed this to run
        // ~indefinitely, one cheap timeout per byte).
        let (mut tx, rx) = pair();
        std::thread::spawn(move || {
            for _ in 0..10 {
                let _ = tx.write_all(b"a");
                std::thread::sleep(Duration::from_millis(150));
            }
        });
        let err = read_capped_frame(&rx, 64 << 10, Duration::from_millis(200)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("deadline") || msg.contains("read failed"), "{msg}");
    }
}
