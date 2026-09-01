// crates/restoreplane-pipeline/src/chain.rs
//! S05 §2~4 — 인코딩 체인: 평문 → blake3 tee → zstd → chacha20poly1305 STREAM → 로컬 스풀.
//!
//! 섹션 편차: 섹션 원본은 age::StreamWriter(std::io::Write)를 쓰지만, 이 프로젝트의
//! restoreplane-secrets는 chacha20poly1305 STREAM(청크 기반 AEAD)만 공개한다. 그래서 age 자리에
//! `StreamEncryptWriter`(청크 버퍼링으로 Write 인터페이스를 STREAM에 맞춤)를 둔다. finish 규율
//! (inner→outer, 명시적, drop 의존 금지)은 섹션과 동일하게 유지한다(P0-2/R3/R5).

use std::io::{BufWriter, Write};

use chacha20poly1305::aead::stream::EncryptorBE32;
use chacha20poly1305::ChaCha20Poly1305;
use restoreplane_secrets::Dek;

/// STREAM 청크 크기(평문 기준). 복호 측(crypto.rs)과 반드시 동일해야 한다.
pub const STREAM_CHUNK: usize = 64 * 1024;
/// chacha20poly1305 STREAM nonce prefix(7B). DEK가 백업마다 새로우므로 상수 재사용이 안전(dek.rs 주석).
pub const NONCE_PREFIX: [u8; 7] = *b"dbbkup1";

// ---------------------------------------------------------------------
// HashingWriter — 평문 스트리밍 blake3 tee (zstd 이전 위치, R1/P1-3)
// ---------------------------------------------------------------------
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            bytes: 0,
        }
    }
    /// 해시/카운트를 뽑고 내부 라이터를 반환(finish 순서에서 zstd로 위임).
    pub fn finalize(self) -> ([u8; 32], u64, W) {
        (*self.hasher.finalize().as_bytes(), self.bytes, self.inner)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?; // 부분 write 대비: 실제 쓰인 n만 해시
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------
// StreamEncryptWriter — chacha20poly1305 STREAM(BE32)을 Write로 감싼 래퍼.
// 평문을 STREAM_CHUNK 단위로 잘라 encrypt_next, finish에서 나머지를 encrypt_last.
// (age::StreamWriter의 대체물 — finish() 명시 호출이 인증 태그를 기록한다, P0-2)
// ---------------------------------------------------------------------
pub struct StreamEncryptWriter<W: Write> {
    inner: W,
    enc: Option<EncryptorBE32<ChaCha20Poly1305>>,
    buf: Vec<u8>,
}

impl<W: Write> StreamEncryptWriter<W> {
    pub fn new(inner: W, dek: &Dek) -> Self {
        Self {
            inner,
            enc: Some(dek.payload_encryptor(&NONCE_PREFIX)),
            buf: Vec::with_capacity(STREAM_CHUNK),
        }
    }

    /// 가득 찬 청크를 encrypt_next로 흘려보낸다. (버퍼에 STREAM_CHUNK 이상이 있을 때)
    fn flush_full_chunks(&mut self) -> std::io::Result<()> {
        while self.buf.len() >= STREAM_CHUNK {
            let chunk: Vec<u8> = self.buf.drain(..STREAM_CHUNK).collect();
            let enc = self
                .enc
                .as_mut()
                .ok_or_else(|| std::io::Error::other("STREAM encryptor already finished"))?;
            let ct = enc
                .encrypt_next(chunk.as_slice())
                .map_err(|e| std::io::Error::other(format!("encrypt_next: {e}")))?;
            self.inner.write_all(&ct)?;
        }
        Ok(())
    }

    /// 명시적 finish: 남은 평문을 encrypt_last로 마감하고 inner를 반환한다.
    /// drop만으로는 마지막 태그가 기록되지 않으므로 반드시 호출해야 한다(P0-2).
    pub fn finish(mut self) -> std::io::Result<W> {
        self.flush_full_chunks()?;
        let enc = self
            .enc
            .take()
            .ok_or_else(|| std::io::Error::other("STREAM encryptor already finished"))?;
        let last = std::mem::take(&mut self.buf);
        let ct = enc
            .encrypt_last(last.as_slice())
            .map_err(|e| std::io::Error::other(format!("encrypt_last: {e}")))?;
        self.inner.write_all(&ct)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for StreamEncryptWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        self.flush_full_chunks()?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        // STREAM은 청크 경계에서만 방출 가능하므로 내부 버퍼는 finish까지 유지한다.
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------
// FinishGuard — debug에서 finish 없이 drop되면 panic(P0-2/R5)
// ---------------------------------------------------------------------
pub struct FinishGuard {
    finished: bool,
}
impl FinishGuard {
    pub fn new() -> Self {
        Self { finished: false }
    }
    pub fn mark(&mut self) {
        self.finished = true;
    }
}
impl Default for FinishGuard {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for FinishGuard {
    fn drop(&mut self) {
        debug_assert!(
            self.finished,
            "pipeline dropped without finish() — 절단 백업 위험(P0-2)"
        );
    }
}

// ---------------------------------------------------------------------
// 타입 별칭 + EncryptChain (S05 §4)
// 쓰기 방향: 평문 → HashingWriter(tee) → zstd → StreamEncrypt → BufWriter<File>(스풀)
// ---------------------------------------------------------------------
type Spool = BufWriter<std::fs::File>;
type CipherW = HashingWriter<Spool>;
type EncW = StreamEncryptWriter<CipherW>;
type ZstdW = zstd::stream::write::Encoder<'static, EncW>;
type ChainW = HashingWriter<ZstdW>;

pub struct EncryptChain {
    tee: Option<ChainW>,
    guard: FinishGuard,
}

impl EncryptChain {
    pub fn new(spool_path: &std::path::Path, dek: &Dek, zstd_level: i32) -> anyhow::Result<Self> {
        // 조립은 스트림 복사 이전이라 `?` 허용(아직 커밋 구간 아님).
        let file = std::fs::File::create(spool_path)?;
        let spool: Spool = BufWriter::new(file);
        let enc_w: EncW = StreamEncryptWriter::new(HashingWriter::new(spool), dek);
        let zstd_w: ZstdW = zstd::stream::write::Encoder::new(enc_w, zstd_level)?;
        Ok(Self {
            tee: Some(HashingWriter::new(zstd_w)),
            guard: FinishGuard::new(),
        })
    }

    /// 복사(pull) / tar(push) 양쪽이 여기로 쓴다.
    pub fn writer(&mut self) -> &mut ChainW {
        self.tee.as_mut().expect("chain writer used after commit")
    }

    /// finish inner→outer + flush + fsync. R3/R4 준수 — 실패는 전부 Err로 수렴시켜
    /// 호출부가 스풀을 삭제하게 한다. 반환: (평문 blake3, 암호문 blake3, 평문 바이트, 암호문 바이트).
    pub fn commit(mut self) -> anyhow::Result<([u8; 32], [u8; 32], u64, u64)> {
        let tee = self.tee.take().expect("chain committed twice");
        let (plaintext_hash, plain_bytes, zstd_w) = tee.finalize(); // 평문 해시 확정
                                                                    // ---- inner→outer, 명시적 finish (P0-2) ----
        let enc_w = zstd_w.finish()?; // (1) zstd 프레임 마감
        let ciphertext_tee = enc_w.finish()?; // (2) STREAM 마지막 태그 기록
        let (ciphertext_hash, ciphertext_bytes, spool) = ciphertext_tee.finalize();
        let mut file = spool
            .into_inner()
            .map_err(|e| anyhow::anyhow!("스풀 flush 실패: {e}"))?;
        file.flush()?;
        file.sync_all()?; // (3) fsync
        anyhow::ensure!(
            file.metadata()?.len() == ciphertext_bytes,
            "ciphertext byte counter does not match committed spool length"
        );
        self.guard.mark(); // 여기까지 오면 커밋 성공
        Ok((
            plaintext_hash,
            ciphertext_hash,
            plain_bytes,
            ciphertext_bytes,
        ))
    }

    /// 복사 실패 시: finish 무의미하므로 guard만 mark하고 drop(파일 삭제는 상위 정리 경로).
    pub fn abort_hint(mut self) {
        self.guard.mark();
        // tee가 Some이면 여기서 drop되며 미완결 체인이 정리된다(파일은 호출부가 remove).
    }
}
