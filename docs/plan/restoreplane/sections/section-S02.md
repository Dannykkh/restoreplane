# S02 — SecretManager (봉투암호화 / 서명 / 복구코드)

> 의존: S01(Config 스키마 + CLI). 레이어 1. Slice 1(walking skeleton)의 하중 리스크 컴포넌트.
> 관련 리뷰: C1(DPAPI 구멍), C2(정적 DEK), C3(복원 위조/RCE), C4(오프라인 브루트포스), P0-5(wrap/payload 분리).

## Background

봉투암호화(Envelope Encryption)는 실제 데이터를 암호화하는 대칭키(DEK)와 그 DEK를 감싸는 키(KEK)를
분리하는 구조다. 이 프로젝트에서 SecretManager는 세 가지 독립적 책임을 갖는다.

1. **DEK 수명주기** — 백업 1건마다 새 32B 랜덤 DEK를 발급한다. 정적 DEK를 쓰면 wrap 1건만 유출돼도
   전 백업이 영구 노출되고(C2 = Confidentiality-Rotation Deadlock), WORM 스토리지라 재암호화가 불가능하다.
   백업별 DEK로 유출 blast radius를 백업 1건으로 한정한다.

2. **DEK wrap/unwrap 3경로** — 하나의 DEK를 서로 다른 3개의 신뢰 경로로 각각 wrapping해
   self-describing 스탠자(stanza)로 저장한다. 어느 한 경로만 살아 있어도 백업을 복구할 수 있다.
   - (a) **Argon2id-KEK**: 마스터 비밀번호에서 유도 — 이식성(다른 PC에서도 비번만 알면 복구).
   - (b) **복구코드-KEK**: init 시 1회 표시되는 >=128bit 고엔트로피 코드 — 비번 분실 대비.
   - (c) **DPAPI Machine-scope**: 비커밋 로컬 keyfile 유도 entropy — 무인(unattended) 스케줄 실행.

3. **출처 인증(Origin Authentication)** — 오프라인 ed25519 개인키로 매니페스트에 서명하고, 복원 시
   공개키로 검증해 미서명/위조 백업을 거부한다(C3). 대칭 AEAD는 **무결성**만 보장하며 DEK 보유자는
   유효한 악성 백업을 위조할 수 있으므로, 무결성과 별개로 비대칭 서명이 필요하다.

**P0-5(가장 중요한 분리 원칙)**: age recipient는 X25519/scrypt 기반이지 Argon2id 대칭키가 아니다.
따라서 "DEK를 wrap하는 계층"과 "payload를 암호화하는 계층"을 코드/타입 수준에서 명확히 분리한다.
- wrap 계층: `chacha20poly1305` 원샷 AEAD로 32B DEK를 감싼다(3 스탠자).
- payload 계층: DEK를 키로 하는 `chacha20poly1305` STREAM(또는 DEK 유도 X25519 age recipient)으로
  덤프 본체를 스트리밍 암호화한다. **이 계층은 S05 파이프라인이 구동**하며, S02는 DEK와 primitive만 제공한다.

## Requirements

| # | 요구사항 | 근거 |
|---|---------|------|
| R1 | 백업 1건당 새 32B 랜덤 DEK를 CSPRNG(OsRng)로 발급 | C2 |
| R2 | DEK를 3경로(Argon2id / 복구코드 / DPAPI)로 wrap, 각 스탠자는 self-describing | 3경로 복구 |
| R3 | Argon2id 파라미터 m=64MiB, t=3, p=1, v0x13; salt+params를 스탠자에 저장 | rust-stack §2 |
| R4 | 복구코드는 >=128bit 엔트로피, init 1회만 표시, 평문 미저장(검증자만 보관) | 비번 분실 복구 |
| R5 | DPAPI entropy는 **하드코딩 금지** — 비커밋 로컬 keyfile에서 HKDF 유도 | C1 |
| R6 | wrap 계층(chacha20poly1305)과 payload 계층(DEK 기반)을 타입으로 분리 | P0-5 |
| R7 | 오프라인 ed25519 개인키로 매니페스트 서명, 복원 시 공개키 검증, 미서명 거부 | C3 |
| R8 | rotate-key: 덤프 재암호화 없이 DEK만 재-wrap, dek_wrap_gen 증가 | 회전 비용 |
| R9 | keys status: 3경로 스탠자 존재/생성세대/DPAPI 등록 머신 상태 출력 | 운영성 |
| R10 | register-dpapi / unlock: 새 PC에서 비번·복구코드로 unlock 후 DPAPI 스탠자 신규 등록 | 이식성 |
| R11 | 모든 평문 키 자료(DEK/KEK/비번/복구코드)는 `zeroize`로 메모리 소거 | 메모리 잔류 |
| R12 | wrap 파일/keyfile/개인키/복구코드는 리포 밖(.gitignore, S11과 정합) | C4/H3 |

## Dependencies

- **S01** — Config 스키마(경로: wrap 파일, keyfile, 공개키), CLI 서브커맨드 정의
  (`init`, `rotate-key`, `keys status`, `register-dpapi`, `unlock`, `restore --decrypt-only`,
  `restore --with-recovery-code`).
- **하위 의존(이 섹션을 소비)**: S05(payload primitive 사용), S08(매니페스트 서명/검증).
- **경계 규칙**: S02는 payload 데이터를 직접 스트리밍하지 않는다. `Dek`와 payload primitive만 넘기고,
  실제 zstd→암호화→스풀 파이프는 S05가 소유한다(P0-5 분리 유지).

## Reference Libraries

| 크레이트 | 버전 | 역할 | 주의 |
|---------|------|------|------|
| `age` | 0.12.1 | payload STREAM 암호화(recipient 모드, DEK 유도 X25519) 옵션 | recipient=X25519/scrypt이며 Argon2id 대칭키 아님(P0-5). `StreamWriter::finish()` 필수 |
| `argon2` | 0.5.3 | Argon2id KEK 유도(m=64MiB,t=3,p=1) | memory cost 단위 KiB(65536=64MiB). params 반드시 영속화 |
| `chacha20poly1305` | 0.10.x | wrap 스탠자 AEAD(원샷) + payload STREAM(대안 경로) | `stream` feature로 `EncryptorBE32`/`DecryptorBE32` |
| `ed25519-dalek` | 2.x | 오프라인 서명/검증(매니페스트) | `SigningKey`/`VerifyingKey`, `pkcs8`/`rand_core` feature |
| `windows-dpapi` | 0.x | DPAPI Machine-scope protect/unprotect | `Scope::Machine` + `pOptionalEntropy`(keyfile 유도). off-box 절도만 방어, 동일 머신 admin은 미방어 |
| `hkdf` | 0.12.x | keyfile bytes → DPAPI entropy, DEK → X25519 유도 | SHA-256 기반 |
| `zeroize` | 1.x | DEK/KEK/비번/복구코드 메모리 소거 | `Zeroizing<T>` 래퍼 + `ZeroizeOnDrop` derive |
| `rand` | 0.8.x | `OsRng`(CSPRNG)로 DEK/복구코드/salt 생성 | `getrandom` 백엔드, 사용자공간 PRNG 금지 |
| `serde` + `serde_json` | 1.x / 1.x | 스탠자/매니페스트 직렬화(canonical) | 서명 대상은 결정적 직렬화 |
| `bech32` 또는 `data-encoding` | - | 복구코드 인코딩(Crockford base32 등 오타 방지) | 사용자 입력 코드 |

> `[확인 필요]`: `windows-dpapi` 0.x의 정확한 entropy 파라미터 시그니처, age 0.12에서 DEK bytes로부터
> `x25519::Identity`를 직접 구성 가능한지. 후자가 불확실하면 payload는 chacha20poly1305 STREAM(아래 구현)으로 확정한다.

## Implementation

### 모듈 구조

```
src/secret/
  mod.rs          // SecretManager 진입점, 공개 API
  dek.rs          // Dek(zeroize), 발급/payload primitive 핸드오프
  wrap.rs         // WrapStanza enum, WrapSet, wrap/unwrap 3경로
  kdf.rs          // Argon2id KEK 유도, 복구코드 생성/검증
  dpapi.rs        // Machine-scope + keyfile-entropy (Windows)
  sign.rs         // ed25519 서명/검증
  rotate.rs       // rotate-key, dek_wrap_gen, register-dpapi/unlock
```

### 핵심 타입 (dek.rs)

```rust
use zeroize::{Zeroize, Zeroizing};
use rand::rngs::OsRng;
use rand::RngCore;

/// 백업 1건당 새로 발급되는 데이터 암호화 키(R1, C2 대응).
/// Drop 시 메모리에서 소거된다.
#[derive(Clone)]
pub struct Dek(Zeroizing<[u8; 32]>);

impl Dek {
    /// CSPRNG(OsRng)로 새 DEK 발급 — 백업마다 호출.
    pub fn generate() -> Self {
        let mut k = [0u8; 32];
        OsRng.fill_bytes(&mut k);
        Dek(Zeroizing::new(k))
    }

    /// wrap 계층 전용 접근자(내부 크레이트 가시성).
    pub(crate) fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    pub(crate) fn from_bytes(b: [u8; 32]) -> Self {
        Dek(Zeroizing::new(b))
    }
}
```

### payload 계층 분리 (P0-5, R6)

S02는 payload를 직접 암호화하지 않고, DEK로부터 **payload primitive**만 만들어 S05에 넘긴다.
이렇게 하면 "wrap = chacha20poly1305 원샷 on 32B" 와 "payload = STREAM on 덤프"가 물리적으로 분리된다.

```rust
use chacha20poly1305::aead::stream::{EncryptorBE32, DecryptorBE32};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};

impl Dek {
    /// payload 암호화용 STREAM Encryptor(S05가 zstd 출력에 감아 사용).
    /// 주의: 이 nonce prefix는 DEK가 백업마다 새로우므로 상수여도 안전(재사용 없음).
    pub fn payload_encryptor(&self, nonce_prefix: &[u8; 7]) -> EncryptorBE32<ChaCha20Poly1305> {
        let cipher = ChaCha20Poly1305::new(self.0.as_ref().into());
        EncryptorBE32::from_aead(cipher, nonce_prefix.into())
    }

    pub fn payload_decryptor(&self, nonce_prefix: &[u8; 7]) -> DecryptorBE32<ChaCha20Poly1305> {
        let cipher = ChaCha20Poly1305::new(self.0.as_ref().into());
        DecryptorBE32::from_aead(cipher, nonce_prefix.into())
    }
}
```

> 대안(plan.md 파이프라인의 age 경로): DEK를 HKDF로 X25519 스칼라로 확장해 age recipient를 만들고
> age `StreamWriter`로 payload를 암호화한다. 이때도 recipient는 **DEK 유도값**이지 Argon2id 대칭키가
> 아니므로 P0-5를 지킨다. age의 identity-from-bytes 구성 가능 여부(`[확인 필요]`)에 따라 확정하며,
> 불확실하면 위 chacha20poly1305 STREAM을 기본으로 한다. 어느 경로든 **wrap 계층과 완전히 별개**다.

### wrap 스탠자 (wrap.rs) — self-describing enum (R2)

```rust
use serde::{Serialize, Deserialize};

/// DEK 1개를 감싼 self-describing 스탠자. 각 경로는 독립적으로 DEK를 복구할 수 있다.
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum WrapStanza {
    /// (a) 마스터 비밀번호 Argon2id-KEK로 wrap.
    Argon2id {
        salt: [u8; 16],
        m_cost: u32,      // KiB (65536 = 64MiB)
        t_cost: u32,      // 3
        p_cost: u32,      // 1
        nonce: [u8; 12],  // chacha20poly1305 nonce
        ct: Vec<u8>,      // 32B DEK + 16B tag
    },
    /// (b) 복구코드 Argon2id-KEK로 wrap(고엔트로피).
    RecoveryCode {
        salt: [u8; 16],
        m_cost: u32,
        t_cost: u32,
        p_cost: u32,
        nonce: [u8; 12],
        ct: Vec<u8>,
    },
    /// (c) DPAPI Machine-scope로 wrap(머신+keyfile 바인딩).
    Dpapi {
        machine_id: String,   // keys status 표기용(어느 PC에서 등록됐는지)
        keyfile_id: String,   // keyfile 지문(hex, entropy 자체 아님)
        blob: Vec<u8>,        // CryptProtectData(DEK, entropy=keyfile 유도)
    },
}

/// 한 백업의 DEK를 감싼 3경로 스탠자 묶음. 매니페스트에 동봉되어 서명된다.
#[derive(Serialize, Deserialize, Clone)]
pub struct WrapSet {
    pub dek_wrap_gen: u32,          // rotate-key 세대(R8)
    pub stanzas: Vec<WrapStanza>,   // 보통 3개(경로별 1개), register-dpapi로 추가될 수 있음
}
```

### Argon2id KEK 유도 (kdf.rs, R3)

```rust
use argon2::{Argon2, Algorithm, Version, Params};
use zeroize::Zeroizing;

pub const M_COST: u32 = 65536; // 64 MiB
pub const T_COST: u32 = 3;
pub const P_COST: u32 = 1;

/// 마스터 비번/복구코드 → 32B KEK. 파라미터는 스탠자에 저장해 재유도 가능하게 함(gotcha).
fn derive_kek(secret: &[u8], salt: &[u8], m: u32, t: u32, p: u32)
    -> Result<Zeroizing<[u8; 32]>, SecretError>
{
    let params = Params::new(m, t, p, Some(32))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut kek = Zeroizing::new([0u8; 32]);
    argon.hash_password_into(secret, salt, kek.as_mut())?;
    Ok(kek)
}
```

wrap/unwrap (chacha20poly1305 원샷, wrap 계층):

```rust
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use chacha20poly1305::aead::Aead;

fn wrap_with_kek(kek: &[u8; 32], dek: &Dek) -> Result<([u8;12], Vec<u8>), SecretError> {
    let cipher = ChaCha20Poly1305::new(kek.into());
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher.encrypt(Nonce::from_slice(&nonce), dek.expose().as_slice())?;
    Ok((nonce, ct))
}

fn unwrap_with_kek(kek: &[u8; 32], nonce: &[u8;12], ct: &[u8]) -> Result<Dek, SecretError> {
    let cipher = ChaCha20Poly1305::new(kek.into());
    let pt = Zeroizing::new(cipher.decrypt(Nonce::from_slice(nonce), ct)?);
    let arr: [u8;32] = pt.as_slice().try_into().map_err(|_| SecretError::BadDekLen)?;
    Ok(Dek::from_bytes(arr))
}
```

복구코드 생성/검증(R4):

```rust
/// >=128bit 엔트로피. 20B(160bit) 랜덤 → Crockford base32 → 그룹 표시(예: XXXX-XXXX-...).
/// init 시 1회만 stdout에 표시하고 평문 저장하지 않는다.
pub fn generate_recovery_code() -> Zeroizing<String> {
    let mut raw = [0u8; 20];
    OsRng.fill_bytes(&mut raw);
    let code = data_encoding::BASE32_NOPAD.encode(&raw); // 오타 방지 알파벳
    raw.zeroize();
    Zeroizing::new(group_dashes(&code)) // XXXXX-XXXXX-... 형태
}
```

### DPAPI keyfile-entropy (dpapi.rs, C1 / R5)

핵심: entropy를 공개 바이너리에 **하드코딩하지 않는다**. init 시 머신마다 비커밋 로컬 keyfile을
생성하고, 그 바이트에서 HKDF로 DPAPI `pOptionalEntropy`를 유도한다.

```rust
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

const KEYFILE_INFO: &[u8] = b"restoreplane-dpapi-entropy-v1";

/// %PROGRAMDATA%\restoreplane\dpapi.key (비커밋, ACL 제한). 없으면 init에서 생성.
fn load_or_create_keyfile(path: &Path) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    if !path.exists() {
        let mut kf = [0u8; 48]; // 384bit
        OsRng.fill_bytes(&mut kf);
        write_locked_file(path, &kf)?;   // ACL: 서비스 계정만, S11과 정합
        kf.zeroize();
    }
    Ok(Zeroizing::new(std::fs::read(path)?))
}

/// keyfile bytes → 32B entropy. keyfile을 못 읽는 프로세스는 unprotect 불가(C1 완화).
fn derive_dpapi_entropy(keyfile: &[u8]) -> Zeroizing<[u8; 32]> {
    let hk = Hkdf::<Sha256>::new(None, keyfile);
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(KEYFILE_INFO, out.as_mut()).expect("32 within okm limit");
    out
}

#[cfg(windows)]
fn dpapi_wrap(dek: &Dek, entropy: &[u8; 32]) -> Result<Vec<u8>, SecretError> {
    // windows-dpapi: Scope::Machine + optional entropy. [확인 필요] 정확한 시그니처
    Ok(windows_dpapi::encrypt(dek.expose(), Some(entropy), windows_dpapi::Scope::Machine)?)
}

#[cfg(windows)]
fn dpapi_unwrap(blob: &[u8], entropy: &[u8; 32]) -> Result<Dek, SecretError> {
    let pt = Zeroizing::new(windows_dpapi::decrypt(blob, Some(entropy), windows_dpapi::Scope::Machine)?);
    let arr: [u8;32] = pt.as_slice().try_into().map_err(|_| SecretError::BadDekLen)?;
    Ok(Dek::from_bytes(arr))
}
```

> 잔여 리스크(문서화): DPAPI Machine-scope는 off-box 절도를 막지만, **동일 머신에서 keyfile을 읽을 수
> 있는 admin/동일 계정 프로세스**는 여전히 unprotect 가능하다. 그래서 (1) 최소권한 서비스 계정,
> (2) keyfile ACL 제한, (3) blob·keyfile 미커밋이 함께 필요하다(C1 3중 대응).

### ed25519 서명/검증 (sign.rs, C3 / R7)

```rust
use ed25519_dalek::{SigningKey, VerifyingKey, Signature, Signer, Verifier};

/// init에서 오프라인 키페어 생성. 개인키는 오프라인/비커밋, 공개키는 config에 핀.
pub fn generate_signing_keypair() -> (SigningKey, VerifyingKey) {
    let sk = SigningKey::generate(&mut OsRng);
    let vk = sk.verifying_key();
    (sk, vk)
}

/// 매니페스트는 결정적(canonical) JSON으로 직렬화 후 서명(S08과 공유하는 canonical 규칙).
pub fn sign_manifest(sk: &SigningKey, canonical_bytes: &[u8]) -> Signature {
    sk.sign(canonical_bytes)
}

/// 복원 시 핀된 공개키로 검증. 실패/미서명이면 복원 거부(Origin Authentication).
pub fn verify_manifest(vk: &VerifyingKey, canonical_bytes: &[u8], sig: &Signature)
    -> Result<(), SecretError>
{
    vk.verify(canonical_bytes, sig).map_err(|_| SecretError::SignatureRejected)
}
```

복원 게이트(Restorer/S09가 호출): 서명 필드가 없거나 검증 실패면 즉시 HARD FAIL,
`--allow-unsigned` 같은 우회 플래그는 제공하지 않는다(대칭 AEAD 무결성만으로는 위조 방어 불가 — C3).

### SecretManager 진입점 (mod.rs)

```rust
pub struct SecretManager {
    signing_key: Option<SigningKey>,   // 서명 시에만 로드(오프라인 개인키)
    verifying_key: VerifyingKey,       // config에 핀된 공개키
    keyfile_path: PathBuf,             // DPAPI entropy용 비커밋 keyfile
    recovery_verifier: RecoveryVerifier, // 복구코드 평문 아닌 검증자(Argon2 해시)
}

impl SecretManager {
    /// 백업 실행 시: 새 DEK 발급 + 3경로 wrap → WrapSet(매니페스트 동봉).
    pub fn issue_and_wrap(
        &self,
        master_pw: &Zeroizing<String>,
        gen: u32,
    ) -> Result<(Dek, WrapSet), SecretError> {
        let dek = Dek::generate();                    // R1
        let mut stanzas = Vec::with_capacity(3);
        stanzas.push(self.wrap_argon2id(&dek, master_pw)?);   // (a)
        stanzas.push(self.wrap_recovery(&dek)?);              // (b) 현재 세대 복구 KEK
        #[cfg(windows)]
        stanzas.push(self.wrap_dpapi(&dek)?);                // (c)
        Ok((dek, WrapSet { dek_wrap_gen: gen, stanzas }))
    }

    /// 복원/rotate 시: 사용 가능한 경로로 DEK 복구(우선순위 DPAPI→비번→복구코드).
    pub fn unwrap(&self, set: &WrapSet, cred: &Credential) -> Result<Dek, SecretError> {
        for st in &set.stanzas {
            match (st, cred) {
                (WrapStanza::Dpapi{blob,..}, Credential::Dpapi) =>
                    return self.unwrap_dpapi(blob),
                (WrapStanza::Argon2id{..}, Credential::Password(pw)) =>
                    return self.unwrap_argon2id(st, pw),
                (WrapStanza::RecoveryCode{..}, Credential::Recovery(code)) =>
                    return self.unwrap_recovery(st, code),
                _ => continue,
            }
        }
        Err(SecretError::NoUsableStanza)
    }
}

pub enum Credential {
    Dpapi,
    Password(Zeroizing<String>),
    Recovery(Zeroizing<String>),
}
```

### rotate-key + dek_wrap_gen (rotate.rs, R8)

덤프 본체(대용량)는 재암호화하지 않는다. DEK(32B)만 새 KEK 세대로 재-wrap하고, WORM 목적지에는
**delete 없이 append**로 새 rewrap 스탠자 객체를 올린다(기존 스탠자는 WORM이라 남지만, unwrap은
최고 `dek_wrap_gen`을 우선).

```rust
impl SecretManager {
    /// 보존 중 백업들의 DEK를 새 비번 KEK로 재-wrap. 덤프는 손대지 않음.
    pub fn rotate_key(
        &self,
        catalog: &Catalog,
        old: &Credential,          // 기존 비번/복구/DPAPI로 unwrap
        new_pw: &Zeroizing<String>,
        next_gen: u32,
    ) -> Result<RotateReport, SecretError> {
        let mut report = RotateReport::default();
        for backup in catalog.active_backups()? {
            let set = load_wrapset_from_manifest(&backup)?;
            let dek = self.unwrap(&set, old)?;                 // 값싼 32B 복구
            let new_stanza = self.wrap_argon2id_gen(&dek, new_pw, next_gen)?;
            let new_set = WrapSet {
                dek_wrap_gen: next_gen,
                stanzas: vec![new_stanza /* + 복구/DPAPI 재-wrap */],
            };
            // append-only Put: rewrap/<backup_id>.gen<N>.json (서명 포함)
            destination_append_rewrap(&backup, &new_set, &self.signing_key)?;
            report.rewrapped += 1;
        }
        Ok(report) // 덤프 재업로드 0건 — 회전 비용 = DEK 크기 * N
    }
}
```

### register-dpapi / unlock (R10)

새 PC는 이전 머신의 DPAPI blob을 복호화할 수 없다(머신+keyfile 바인딩). 따라서:

```rust
impl SecretManager {
    /// 새 PC: 비번 또는 복구코드로 DEK를 unlock(무인 DPAPI 경로 없이).
    pub fn unlock(&self, set: &WrapSet, cred: &Credential) -> Result<Dek, SecretError> {
        self.unwrap(set, cred) // Password 또는 Recovery만 성공
    }

    /// 새 PC에서 무인 실행을 켜려면: 로컬 keyfile 신규 생성 + 현재 세대 DEK에 DPAPI 스탠자 추가.
    pub fn register_dpapi(
        &self,
        catalog: &Catalog,
        unlock_cred: &Credential, // 비번/복구코드로 먼저 unlock
    ) -> Result<(), SecretError> {
        #[cfg(windows)]
        for backup in catalog.active_backups()? {
            let set = load_wrapset_from_manifest(&backup)?;
            let dek = self.unlock(&set, unlock_cred)?;
            let dpapi_stanza = self.wrap_dpapi(&dek)?;         // 새 머신 keyfile entropy
            destination_append_stanza(&backup, dpapi_stanza, &self.signing_key)?;
        }
        Ok(())
    }
}
```

### keys status (R9)

```
$ restoreplane keys status
Signing:     ed25519 pub 핀됨 (fingerprint 3f9a..) / 개인키: 오프라인(로컬 없음)
Wrap 경로:   [x] Argon2id(비번)  [x] 복구코드  [x] DPAPI
DEK 세대:    dek_wrap_gen = 2 (rotate-key 2회)
DPAPI 등록:  이 머신(HOST-A, keyfile 지문 7c1e..) = 등록됨
             HOST-B = 미등록 → register-dpapi 필요
복구코드:    등록됨(검증자 존재). 원본 코드는 표시되지 않음.
```

### zeroize 규율 (R11)

- `Dek`, KEK(`Zeroizing<[u8;32]>`), 비번/복구코드(`Zeroizing<String>`), keyfile bytes, HKDF 출력,
  복호 평문(`Zeroizing<Vec<u8>>`)은 모두 Drop 시 소거.
- 로그/에러 메시지에 키 자료를 절대 포함하지 않는다(Debug derive 금지, 스탠자만 `ct` 표시).
- `SigningKey`는 서명 순간에만 로드하고 스코프 종료 시 drop(ed25519-dalek은 내부적으로 소거).

## Test Scenarios

| # | 시나리오 | 기대 |
|---|---------|------|
| T1 | DEK 발급 2회 | 서로 다른 32B(정적 아님, C2) |
| T2 | Argon2id wrap → unwrap | 동일 DEK 복구, 잘못된 비번은 AEAD 실패 |
| T3 | 복구코드 wrap → unwrap | 정확 코드만 성공, 1글자 변경 시 실패 |
| T4 | DPAPI wrap → unwrap(동일 머신+keyfile) | 성공 |
| T5 | DPAPI blob을 다른 keyfile로 unwrap | 실패(C1: entropy 없으면 복호 불가) |
| T6 | payload STREAM 암/복호 라운드트립(대용량 스트리밍) | 평문 일치, finish 누락 시 복호 실패 감지 |
| T7 | payload와 wrap 키 교차 사용 시도 | 타입/키 불일치로 컴파일 또는 런타임 거부(P0-5) |
| T8 | 매니페스트 서명 → 검증 | 유효 서명 통과 |
| T9 | 매니페스트 1바이트 변조 후 검증 | 거부(SignatureRejected, C3) |
| T10 | 미서명 매니페스트 복원 시도 | HARD FAIL, 우회 플래그 없음 |
| T11 | rotate-key 후 신·구 비번으로 unwrap | 신 세대는 새 비번, 구 스탠자는 구 비번(덤프 재암호화 0) |
| T12 | 새 PC에서 unlock(비번) 후 register-dpapi | DPAPI 스탠자 추가, 이후 무인 unwrap 성공 |
| T13 | 3경로 중 2개 손상 시 남은 1개로 복구 | 성공(독립성) |
| T14 | Drop 후 메모리 스캔(가능 범위) | 키 자료 잔류 없음(zeroize) |
| T15 | Argon2 params 변경 후 기존 스탠자 unwrap | 스탠자 저장 params로 재유도 성공(gotcha 방지) |

## Implementation Strategy

1. **kdf.rs + wrap.rs 먼저** — Argon2id KEK, chacha20poly1305 원샷 wrap/unwrap, 복구코드 생성.
   순수 크로스플랫폼 유닛 테스트(T1~T3)로 하중 리스크 조기 검증.
2. **dek.rs payload primitive** — chacha20poly1305 STREAM 라운드트립(T6). age recipient 경로는
   `[확인 필요]` 해소 후 선택적 추가.
3. **sign.rs** — ed25519 서명/검증(T8~T10). S08 canonical 직렬화 규칙과 계약 합의.
4. **dpapi.rs(Windows)** — keyfile-entropy 유도 + Machine-scope(T4/T5). `#[cfg(windows)]` 격리,
   비Windows는 stub로 컴파일만.
5. **rotate.rs** — rotate-key/register-dpapi/unlock/keys status(T11~T13). Catalog(S08) 연동.
6. **zeroize 감사** — 모든 키 경로에 `Zeroizing`/`ZeroizeOnDrop` 적용 확인(T14).

## Quality Gate

- [ ] `cargo test -p secret` 전 시나리오(T1~T15) 통과.
- [ ] wrap 계층과 payload 계층이 서로의 키 타입을 받을 수 없음(타입 분리 컴파일 보장, P0-5).
- [ ] 어떤 코드 경로에도 하드코딩된 entropy/키/salt 없음(grep + cargo-deny, S11 정합).
- [ ] `Debug`가 `Dek`/KEK/비번/복구코드에 파생되지 않음(clippy lint).
- [ ] Argon2 params(m=64MiB,t=3,p=1)가 스탠자에 저장되고 재유도로 검증됨.
- [ ] 미서명/변조 매니페스트가 복원에서 거부됨(우회 플래그 부재 확인).
- [ ] 비Windows 타깃에서도 DPAPI 제외 나머지 컴파일 성공.

## Risk & Rollback

| 리스크 | 완화 | 롤백 |
|-------|------|------|
| age 0.12 identity-from-DEK 불가(`[확인 필요]`) | payload를 chacha20poly1305 STREAM으로 확정 | age 경로 미채택, 코드 변경 없음 |
| windows-dpapi entropy 시그니처 상이 | `windows` 크레이트 `CryptProtectData` 직접 호출로 폴백 | dpapi.rs 내부 교체, 인터페이스 불변 |
| DPAPI 동일 머신 admin 미방어(C1 잔여) | 최소권한 계정 + keyfile ACL + 미커밋 3중 방어, 문서화 | 무인 경로 비활성 시 비번/복구코드 경로만 사용 |
| rotate-key가 WORM에서 구 스탠자 삭제 불가 | append 새 세대 + 최고 dek_wrap_gen 우선 unwrap | 구 세대 유지(보안 저하 없음, 구 비번도 여전히 유효) |
| ed25519 개인키 유실 | 오프라인 백업 매체 2곳 보관 권고 | 신규 키페어 발급 + 공개키 재핀(과거 서명은 구 공개키로만 검증) |
| 복구코드 분실 + 비번 분실 + DPAPI 머신 손실 동시 | 3경로 독립성으로 확률 최소화, keys status로 상시 가시화 | 복구 불가(설계상 한계, init 시 경고 출력) |

## Acceptance Criteria

- AC1: 백업마다 새 DEK가 발급되고(T1), 하나의 DEK/스탠자 유출이 다른 백업에 전파되지 않는다(C2).
- AC2: DEK가 Argon2id / 복구코드 / DPAPI 3경로로 wrap되며, 각각 독립적으로 DEK를 복구한다(T2~T4, T13).
- AC3: DPAPI entropy가 비커밋 keyfile에서 유도되어 hardcode가 전혀 없고, keyfile 없이는 복호 불가(T5, C1).
- AC4: wrap 계층(chacha20poly1305 원샷)과 payload 계층(DEK STREAM/age)이 코드/타입으로 분리된다(T7, P0-5).
- AC5: 매니페스트가 오프라인 ed25519로 서명되고, 미서명/변조 백업은 복원에서 거부된다(T9/T10, C3).
- AC6: rotate-key가 덤프 재암호화 없이 DEK만 재-wrap하고 dek_wrap_gen을 올린다(T11, R8).
- AC7: 새 PC에서 unlock 후 register-dpapi로 무인 실행이 복구되고, keys status가 상태를 정확히 보고한다(T12, R9/R10).
- AC8: 모든 평문 키 자료가 Drop 시 zeroize되고, 로그/Debug에 노출되지 않는다(T14, R11).

## Files

| 파일 | 내용 |
|------|------|
| `src/secret/mod.rs` | `SecretManager`, `Credential`, 공개 API, issue_and_wrap/unwrap |
| `src/secret/dek.rs` | `Dek`(zeroize), 발급, payload primitive(STREAM enc/dec) |
| `src/secret/wrap.rs` | `WrapStanza` enum, `WrapSet`, chacha20poly1305 wrap/unwrap 3경로 |
| `src/secret/kdf.rs` | Argon2id KEK 유도, 복구코드 생성/검증(RecoveryVerifier) |
| `src/secret/dpapi.rs` | `#[cfg(windows)]` Machine-scope + keyfile-entropy HKDF |
| `src/secret/sign.rs` | ed25519 키생성/서명/검증, canonical 직렬화 계약(S08 공유) |
| `src/secret/rotate.rs` | rotate-key, register-dpapi, unlock, keys status, RotateReport |
| `tests/secret_roundtrip.rs` | T1~T15 통합 테스트 |
| `.gitignore`(S11) | wrap 파일, dpapi.key, ed25519 개인키, 복구코드 로그 제외(C4/R12) |

`#tags: secretmanager, envelope-encryption, dek, argon2id, dpapi, ed25519, rotate-key, zeroize, rust`
