use std::fmt;
use std::io::{self, Read};

use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Eq, PartialEq)]
pub struct CapturedStream {
    tail: Vec<u8>,
    total_bytes: u64,
    truncated: bool,
}

impl CapturedStream {
    pub fn tail(&self) -> &[u8] {
        &self.tail
    }

    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl fmt::Debug for CapturedStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedStream")
            .field("tail_bytes", &self.tail.len())
            .field("total_bytes", &self.total_bytes)
            .field("truncated", &self.truncated)
            .finish()
    }
}

pub(crate) fn capture_reader(
    mut reader: impl Read,
    tail_capacity: usize,
    literals: Vec<Zeroizing<Vec<u8>>>,
) -> io::Result<CapturedStream> {
    let mut tail = TailRing::new(tail_capacity);
    let mut redactor = LiteralRedactor::new(literals);
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        redactor.push(&buffer[..read], &mut tail);
    }
    redactor.finish(&mut tail);
    Ok(tail.finish())
}

struct LiteralRedactor {
    literals: Vec<Zeroizing<Vec<u8>>>,
    carry: Zeroizing<Vec<u8>>,
    retained_suffix_bytes: usize,
}

impl LiteralRedactor {
    fn new(mut literals: Vec<Zeroizing<Vec<u8>>>) -> Self {
        literals.sort_by_key(|literal| std::cmp::Reverse(literal.len()));
        let retained_suffix_bytes = literals
            .iter()
            .map(|literal| literal.len().saturating_sub(1))
            .max()
            .unwrap_or(0);
        Self {
            literals,
            carry: Zeroizing::new(Vec::with_capacity(retained_suffix_bytes)),
            retained_suffix_bytes,
        }
    }

    fn push(&mut self, bytes: &[u8], tail: &mut TailRing) {
        if self.literals.is_empty() {
            tail.push(bytes);
            return;
        }
        let mut combined = Zeroizing::new(Vec::with_capacity(self.carry.len() + bytes.len()));
        combined.extend_from_slice(&self.carry);
        combined.extend_from_slice(bytes);
        redact_literals(&mut combined, &self.literals);
        let retained = self.retained_suffix_bytes.min(combined.len());
        let emitted = combined.len() - retained;
        tail.push(&combined[..emitted]);
        self.carry.zeroize();
        self.carry.clear();
        self.carry.extend_from_slice(&combined[emitted..]);
    }

    fn finish(&mut self, tail: &mut TailRing) {
        tail.push(&self.carry);
        self.carry.zeroize();
        self.carry.clear();
    }
}

fn redact_literals(bytes: &mut [u8], literals: &[Zeroizing<Vec<u8>>]) {
    for literal in literals {
        if literal.len() > bytes.len() {
            continue;
        }
        let mut offset = 0_usize;
        while offset + literal.len() <= bytes.len() {
            if bytes[offset..offset + literal.len()] == literal[..] {
                bytes[offset..offset + literal.len()].fill(b'*');
                offset += literal.len();
            } else {
                offset += 1;
            }
        }
    }
}

struct TailRing {
    bytes: Zeroizing<Vec<u8>>,
    capacity: usize,
    length: usize,
    next: usize,
    total_bytes: u64,
}

impl TailRing {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: Zeroizing::new(vec![0_u8; capacity]),
            capacity,
            length: 0,
            next: 0,
            total_bytes: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        for byte in bytes {
            self.bytes[self.next] = *byte;
            self.next = (self.next + 1) % self.capacity;
            self.length = (self.length + 1).min(self.capacity);
        }
    }

    fn finish(self) -> CapturedStream {
        let mut tail = Vec::with_capacity(self.length);
        if self.length < self.capacity {
            tail.extend_from_slice(&self.bytes[..self.length]);
        } else {
            tail.extend_from_slice(&self.bytes[self.next..]);
            tail.extend_from_slice(&self.bytes[..self.next]);
        }
        CapturedStream {
            tail,
            total_bytes: self.total_bytes,
            truncated: self.total_bytes > self.capacity as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use zeroize::Zeroizing;

    use super::capture_reader;

    #[test]
    fn overlapping_literals_are_redacted_longest_first_across_read_boundaries() {
        let reader = ChunkReader::new(vec![b"top-sec".to_vec(), b"ret-value".to_vec()]);
        let captured = capture_reader(
            reader,
            64,
            vec![
                Zeroizing::new(b"secret".to_vec()),
                Zeroizing::new(b"secret-value".to_vec()),
            ],
        )
        .expect("capture");
        assert_eq!(captured.tail(), b"top-************");
    }

    struct ChunkReader {
        chunks: std::vec::IntoIter<Vec<u8>>,
    }

    impl ChunkReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into_iter(),
            }
        }
    }

    impl Read for ChunkReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.next() else {
                return Ok(0);
            };
            output[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }
}
