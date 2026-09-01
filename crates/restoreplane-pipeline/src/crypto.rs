// crates/restoreplane-pipeline/src/crypto.rs
//! 스풀 라운드트립 복호 경로(verify L1 및 테스트에서 사용).
//! 스풀 → chacha20poly1305 STREAM decrypt → zstd decode → 원본 평문(P1-3, S05 T7).
//!
//! 인코딩 측(chain.rs StreamEncryptWriter)과 청크 프레이밍이 정확히 대칭이어야 한다:
//! 각 encrypt_next 블록 = STREAM_CHUNK + 16(태그), 마지막 encrypt_last 블록 < STREAM_CHUNK + 16.

use std::io::{BufRead, BufReader, Read};

use restoreplane_secrets::Dek;

use crate::chain::{NONCE_PREFIX, STREAM_CHUNK};

const TAG_LEN: usize = 16;
const CIPHER_CHUNK: usize = STREAM_CHUNK + TAG_LEN;

/// buf를 EOF 또는 가득 찰 때까지 채운다. 실제 읽은 바이트 수 반환.
fn read_fully<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// 스풀 파일을 복호+압축해제하여 원본 평문을 복원한다.
pub fn decrypt_and_decompress(spool_path: &std::path::Path, dek: &Dek) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(spool_path)?;
    let mut reader = BufReader::with_capacity(CIPHER_CHUNK * 2, file);
    let mut dec = Some(dek.payload_decryptor(&NONCE_PREFIX));

    // STREAM 복호 결과(=zstd 압축 스트림)를 모은다.
    let mut compressed: Vec<u8> = Vec::new();
    loop {
        let mut block = vec![0u8; CIPHER_CHUNK];
        let n = read_fully(&mut reader, &mut block)?;
        block.truncate(n);
        // 다음 데이터가 남아있는지 peek → 현재 블록이 마지막인지 판정.
        let more = !reader.fill_buf()?.is_empty();
        if more {
            let d = dec
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("STREAM decryptor already finished"))?;
            let pt = d
                .decrypt_next(block.as_slice())
                .map_err(|e| anyhow::anyhow!("decrypt_next 실패(변조/절단 가능): {e}"))?;
            compressed.extend_from_slice(&pt);
        } else {
            let d = dec
                .take()
                .ok_or_else(|| anyhow::anyhow!("STREAM decryptor already finished"))?;
            let pt = d
                .decrypt_last(block.as_slice())
                .map_err(|e| anyhow::anyhow!("decrypt_last 실패(변조/절단 가능): {e}"))?;
            compressed.extend_from_slice(&pt);
            break;
        }
    }

    // zstd 압축 해제 → 원본 평문.
    let mut plaintext = Vec::new();
    zstd::stream::copy_decode(&compressed[..], &mut plaintext)?;
    Ok(plaintext)
}
