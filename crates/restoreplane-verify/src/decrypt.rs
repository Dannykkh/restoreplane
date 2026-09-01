// S09 9.2 — 복호 체인. restoreplane-pipeline 인코딩 체인(평문 → zstd → chacha20poly1305 STREAM)의
// 정확한 역순이다. 청크 상수(STREAM_CHUNK/NONCE_PREFIX)는 pipeline::chain에서 공유해 대칭성을 고정한다.
//
// 인코더는 각 full 청크를 encrypt_next(정확히 STREAM_CHUNK+16 바이트), 마지막을 encrypt_last
// (항상 STREAM_CHUNK+16 미만)로 낸다. 따라서 "정확히 CIPHER_CHUNK 크기인 블록 = next, 그보다 작은
// 마지막 블록 = last"로 판정할 수 있다(pipeline crypto.rs의 peek 로직과 동일 결과).

use restoreplane_pipeline::chain::{NONCE_PREFIX, STREAM_CHUNK};
use restoreplane_secrets::Dek;

const TAG_LEN: usize = 16;
const CIPHER_CHUNK: usize = STREAM_CHUNK + TAG_LEN;

/// 암호문 바이트 → (STREAM 복호 + zstd 디코드) → 원본 평문. 절단/변조는 STREAM 인증 실패로 잡힌다.
pub fn decrypt_and_decompress_bytes(cipher: &[u8], dek: &Dek) -> anyhow::Result<Vec<u8>> {
    let mut dec = Some(dek.payload_decryptor(&NONCE_PREFIX));
    let mut compressed: Vec<u8> = Vec::new();

    let total = cipher.len();
    let mut pos = 0usize;
    loop {
        let take = (total - pos).min(CIPHER_CHUNK);
        let block = &cipher[pos..pos + take];
        pos += take;
        let is_last = pos >= total;
        if !is_last {
            let d = dec
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("STREAM decryptor already finished"))?;
            let pt = d
                .decrypt_next(block)
                .map_err(|e| anyhow::anyhow!("decrypt_next 실패(변조/절단 가능): {e}"))?;
            compressed.extend_from_slice(&pt);
        } else {
            let d = dec
                .take()
                .ok_or_else(|| anyhow::anyhow!("STREAM decryptor already finished"))?;
            let pt = d
                .decrypt_last(block)
                .map_err(|e| anyhow::anyhow!("decrypt_last 실패(변조/절단 가능): {e}"))?;
            compressed.extend_from_slice(&pt);
            break;
        }
    }

    let mut plaintext = Vec::new();
    zstd::stream::copy_decode(&compressed[..], &mut plaintext)?;
    Ok(plaintext)
}

/// 복호된 평문 덤프의 형태. 슬라이스 1은 단일 스트림(custom/plain)만 다룬다.
pub enum PlaintextDump {
    /// custom/plain: 단일 바이트 스트림(컨테이너 stdin으로 파이프하거나 파일로 산출).
    Bytes(Vec<u8>),
    /// directory: untar된 디렉토리(컨테이너 내부 경로 권장). 슬라이스 1 범위 밖(후속).
    Directory(std::path::PathBuf),
}

/// dump_format에 따라 평문 바이트를 적절한 PlaintextDump로 감싼다.
pub fn materialize_dump(plaintext: Vec<u8>, dump_format: &str) -> anyhow::Result<PlaintextDump> {
    match dump_format {
        "directory" => {
            anyhow::bail!("directory 포맷 복원은 후속 슬라이스(컨테이너-내부 untar 경로)")
        }
        _ => Ok(PlaintextDump::Bytes(plaintext)), // custom | plain
    }
}
