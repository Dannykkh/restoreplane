# S07 — SshDest + Tunnel (russh SFTP / host-key 핀)

> 의존: S06(Destination trait / DestCaps / DestWriter / upload_resumable / assert_job_immutability), S01(Config `[[ssh]]`/`[[tunnels]]`), S02(SecretManager — passphrase/password `enc:` unwrap)
> 레이어: 4 · plan 슬라이스 5(“SSH 목적지 + russh 터널 + 옵션 C 팬아웃 3목적지 + prune/rotate-key + doctor”). SSH/터널은 정당한 fast-follow(plan §구현 슬라이스).

## Background

이 섹션은 두 개의 서로 다른 russh 사용처를 담는다. 이름은 비슷하지만 방향과 책임이 다르다.

1. **Tunnel(터널)** — 바스티온(점프 호스트) 뒤에 있는 DB에 닿기 위한 **경유 채널**이다. russh로
   바스티온에 접속한 뒤 `direct-tcpip` 채널을 열어 DB(host:port)로 연결하고, 그 채널을 로컬
   `TcpListener`에 브리지한다. 그러면 pg_dump/mysqldump(및 S03/S04의 BackupSource)는 `127.0.0.1:<로컬포트>`
   로 접속하는 것처럼 동작하고, 실제 트래픽은 바스티온을 통해 DB로 흐른다. 도메인사전의 **Tunnel** =
   “russh direct-tcpip로 바스티온 경유 DB 접속”이 이것이다. 이는 **Destination이 아니라** 백업 소스가
   DB에 닿는 경로를 여는 인프라다.

2. **SshDest(목적지)** — 백업본이 착지하는 **원격 SFTP 저장처**다. S06의 `Destination` trait를
   구현하며, russh의 **SFTP 서브시스템**(`russh-sftp` 클라이언트)으로 스풀 파일을 업로드한다.
   OpenDAL의 sftp 백엔드는 **Unix 전용**(openssh→tokio-pipe 의존, Windows 빌드 불가, rust-stack §5)이므로
   Windows 호스트에서는 **반드시 russh 경로**로 SFTP를 처리한다. 이 이원화가 이 섹션의 구조적 결정이다.

두 사용처 모두의 하중은 **host-key 검증 정책**이다. russh는 저수준이라 `client::Handler`를 직접 구현하고
호스트 키 수용 여부를 스스로 결정해야 한다(rust-stack §4). 레드팀 **H6**과 아키텍처 **P2-1**이 못박은 대로,
**모르는/바뀐 키를 무비판 수용(blind-accept)하면 DB 터널이 그대로 MITM 통로**가 된다(자격증명 + 데이터
탈취). 따라서 정책은 **STRICT 핀**이다: config `[[ssh]] host_key_fpr`에 박힌 지문과 **정확히 일치할 때만**
수용하고, 미지·불일치 키는 **연결 거부**한다. TOFU(첫 접속 자동 신뢰)나 accept-any는 이 프로젝트에서
금지한다.

불변성 측면에서 SshDest는 Object Lock을 강제할 수 없다. `DestCaps.immutability`는 **None**(일반 SFTP) 또는
**ServerAppendOnly**(서버측 chattr +a / SFTP write-only 정책이 **외부에서** 보장될 때만)로 보고한다. SshDest
자신은 WORM을 만들 수 없으므로, **Governance/Compliance를 요구하는 Job은 이 목적지에서 반드시 실패**해야
한다(S06 `assert_job_immutability`, P1-2). SshDest를 “랜섬웨어 방어 목적지”로 오인하지 않게 하는 것이
캡 보고의 핵심이다.

## Requirements

| # | 요구 | 근거 |
|---|------|------|
| R1 | **STRICT host-key 핀**: `client::Handler::check_server_key`가 서버 공개키 지문을 config `host_key_fpr`와 비교, **정확 일치 시에만 수용**. 미지/불일치는 거부 | H6, P2-1, rust-stack §4 |
| R2 | 지문은 표준 형식(`SHA256:...` base64, `ssh-keygen -lf`와 동일)으로 계산·비교. 알고리즘/포맷 명시 | P2-1 |
| R3 | **Tunnel**: 바스티온 접속 → `channel_open_direct_tcpip(db_host, db_port, ...)` → 로컬 `TcpListener` 브리지. BackupSource가 `127.0.0.1:<local_port>`로 접속 | plan §트레이트(Tunnel), rust-stack §4 |
| R4 | 채널↔로컬소켓 **양방향 복사**(`tokio::io::copy_bidirectional`), 연결 종료/에러 전파, 다중 연결 accept 루프 | rust-stack §4 |
| R5 | **SshDest**가 S06 `Destination` 구현: `writer(key)`(SFTP `create`+스트리밍 write), `capabilities()`, `self_test()` | plan §트레이트, S06 계약 |
| R6 | SFTP 업로드는 S06의 **multipart-only-commit 의미론에 정합**: 임시 key로 write → flush → **성공 시에만 최종 key로 rename**(SFTP엔 진짜 멀티파트가 없으므로 rename-commit) | S06 R6, P0-3 |
| R7 | **인증**: 공개키(파일/에이전트) 또는 password. 개인키 passphrase 및 password는 `enc:`로 저장, S02 SecretManager가 unwrap | plan §봉투암호화, P2-1 |
| R8 | `DestCaps`: immutability = **None** 또는 **ServerAppendOnly**(외부 보장 시). checksum_required=false, resumable=true(스풀 재읽기+rename) | S06 P1-2, plan §랜섬웨어 |
| R9 | **Governance/Compliance 요구 Job은 SshDest에서 실패**(assert_job_immutability). SshDest는 Object Lock 불가 명시 | P1-2, task |
| R10 | russh/russh-sftp **정확 버전 핀**(`=`). minor API churn 방지(rust-stack §4 gotcha) | H6, rust-stack §4 |
| R11 | **KeyChanged 특례**: 지문이 이전 값과 달라졌으면 단순 “미지”가 아니라 **MITM 경고**로 요란히 구분해 거부 | H6, russh `Error::KeyChanged` |
| R12 | 로그 시크릿 스크럽(M6): 호스트/포트/유저/키는 되나 password/passphrase/개인키/DEK는 로그 금지 | red-team M6 |
| R13 | russh 0.62 정확 시그니처(`channel_open_direct_tcpip` 인자, `check_server_key` 반환, `into_stream`)는 `[확인 필요]`로 표기 후 빌드 시 실측 | rust-stack §4, task |

## Dependencies

- **입력(S01 Config)**:
  - `[[tunnels]]` — `name`, `bastion_host`, `bastion_port`(기본 22), `bastion_user`, `host_key_fpr`(SHA256 핀,
    P2-1), `auth`(key/password), `identity_file`/`passphrase`(`enc:`) 또는 `password`(`enc:`),
    `db_host`, `db_port`, `local_bind`(기본 `127.0.0.1:0` = OS 할당).
  - `[[ssh]]`(= SshDest) — `name`, `host`, `port`, `user`, `host_key_fpr`, `auth`, `identity_file`/
    `passphrase`(`enc:`)/`password`(`enc:`), `remote_dir`, `append_only`(bool, ServerAppendOnly 자가주장 여부).
- **입력(S02 SecretManager)**: `enc:` 필드(개인키 passphrase, password) unwrap. 개인키 자체는 파일 경로 참조
  또는 `enc:` blob.
- **입력(S06)**: `Destination` trait, `DestCaps`/`ImmutabilityKind`/`DestWriter` 계약, `upload_resumable`의
  커밋 원자성 규율(SshDest는 rename-commit으로 이를 만족), `assert_job_immutability`(R9).
- **출력 소비(S05 fan_out)**: S05 팬아웃은 SshDest도 다른 Destination과 동일하게 `upload_resumable(&spool)`로
  다룬다. Tunnel은 **S03/S04 BackupSource가 DB에 접속하기 전** 열려 있어야 하므로, run 시퀀스에서 dump보다
  선행해 `TunnelHandle`을 세운다(§Implementation 6).
- **출력 소비(S08 Catalog/Manifest)**: 목적지별 `object_key`(원격 경로), 업로드 시각, `immutability`
  (None/ServerAppendOnly)를 기록. SshDest는 서버측 checksum이 없으므로 object_checksum은 로컬 재계산 값 또는
  공란(`[확인 필요]` provider별).

## Reference Libraries (russh 0.62.2 pinned)

| 크레이트 | 버전 | 이 섹션에서의 역할 | Gotcha |
|---------|------|------|--------|
| `russh` | =0.62.2 | SSH 클라이언트(터널 + SFTP 전송 채널). `client::connect`, `client::Handler`, `channel_open_direct_tcpip`, `channel_open_session`+`request_subsystem` | **API churn** → `=` 핀 필수(H6). `check_server_key` **직접 구현 안 하면 accept-any = MITM** |
| `russh-sftp` | =2.x | SFTP v3 클라이언트. `SftpSession::new(channel.into_stream())`, `create`/`open_with_flags`/`rename`/`write_all` | russh 채널을 `into_stream()`으로 받는다. 버전 호환 russh 0.62와 맞물림 `[확인 필요]` |
| `ssh-key` | (russh 재수출) | 서버 공개키 `PublicKey`, `fingerprint(HashAlg::Sha256)`, 개인키 로드 | 지문 알고리즘 `Sha256` 고정(R2) |
| `tokio` | 1.x | `TcpListener`, `TcpStream`, `io::copy_bidirectional`, `AsyncReadExt/WriteExt` | 터널 브리지 |
| `bytes` | 1.x | 스풀 청크 전송 버퍼 | |
| `async-trait` | 0.1 | S06 `Destination` async trait 구현 | |
| `anyhow`/`thiserror`/`tracing` | — | 오류·구조적 로깅(시크릿 스크럽 — M6/R12) | password/passphrase/개인키 로그 금지 |

`Cargo.toml` (발췌):
```toml
[dependencies]
russh = "=0.62.2"                  # API churn 방지 정확 핀(H6). rust-stack §4
russh-sftp = "=2.0.8"             # [확인 필요] russh 0.62 호환 정확 버전. SFTP v3 클라이언트
tokio = { version = "1", features = ["net", "io-util", "rt-multi-thread", "macros"] }
bytes = "1"
async-trait = "0.1"
anyhow = "1"; thiserror = "2"; tracing = "0.1"
# ssh-key는 russh가 재수출(러shr::keys) — 별도 핀 시 russh와 동일 계열로 맞춤 [확인 필요]
```
> **핀 규율(H6)**: russh/russh-sftp는 보안 임계 + minor마다 API가 흔들리므로 `=`로 정확 고정하고
> `Cargo.lock` 커밋. cargo-deny/cargo-audit(S11)로 advisory 게이트. openssl 유입 0(순수 Rust라 원천 없음이
> russh 선택의 결정적 이유 — rust-stack §4).

## Implementation

### 1. host-key 핀 정책 — 이 섹션의 하중 (R1/R2/R11)

STRICT 핀은 **하나의 재사용 Handler**로 구현하고, 터널과 SshDest가 공유한다. 정책 판정은 지문 비교 단
한 곳(`HostKeyPolicy::verify`)에 모은다.

```rust
use russh::keys::ssh_key::{PublicKey, HashAlg};

/// config에서 온 고정 지문. 형식: "SHA256:<base64>" (ssh-keygen -lf 와 동일, R2).
#[derive(Clone, Debug)]
pub struct HostKeyPolicy {
    pub pinned_fpr: String,     // 예: "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU"
    pub label: String,          // 로그용(host:port) — 시크릿 아님(R12)
}

#[derive(Debug, thiserror::Error)]
pub enum HostKeyError {
    #[error("미지의 host key: {label} 지문={got} (핀={want}) — 연결 거부")]
    Unknown { label: String, got: String, want: String },
    #[error("host key가 바뀜(MITM 가능): {label} 지문={got} ≠ 핀={want} — 연결 거부")]
    Changed { label: String, got: String, want: String },
}

impl HostKeyPolicy {
    /// 서버가 제시한 공개키의 지문을 핀과 비교. 정확 일치만 통과(R1).
    pub fn verify(&self, server_key: &PublicKey) -> Result<(), HostKeyError> {
        // ssh-key: fingerprint(Sha256) → Display가 "SHA256:<base64>" 형식(R2).
        let got = server_key.fingerprint(HashAlg::Sha256).to_string();
        if got == self.pinned_fpr {
            tracing::info!(label = %self.label, "host key 지문 일치 — 수용");
            return Ok(());
        }
        // 핀이 비어있지 않은데 불일치면, "미지"가 아니라 "바뀜/위조"로 취급(R11).
        // (첫 접속이든 재접속이든 우리는 항상 핀을 갖고 있으므로 불일치=위험 신호)
        Err(HostKeyError::Changed {
            label: self.label.clone(), got, want: self.pinned_fpr.clone(),
        })
    }
}
```

> **왜 “미지”를 “바뀜”처럼 다루나(R11)**: 이 도구는 **항상 config 핀을 갖고 접속**한다(TOFU 없음). 따라서
> 불일치는 언제나 “내가 아는 키가 아니다”이며, MITM/키교체와 구분 불가하게 **거부**가 맞다. russh 자체도
> `Error::UnknownKey`/`Error::KeyChanged`를 구분해 주지만, 우리 정책층에서는 **둘 다 hard fail**로 수렴한다.
> 운영자가 정당하게 서버 키를 교체했다면 **config의 `host_key_fpr`를 명시적으로 갱신**해야 한다(감사 가능).

### 2. client::Handler 구현 — accept-any 금지 (R1)

```rust
use russh::client::{self, Handler, Session};
use russh::keys::ssh_key::PublicKey;

/// 터널과 SshDest가 공유하는 클라이언트 핸들러. 유일한 보안 책임 = host key 핀 검증.
pub struct PinnedClientHandler {
    policy: HostKeyPolicy,
}

impl PinnedClientHandler {
    pub fn new(policy: HostKeyPolicy) -> Self { Self { policy } }
}

#[async_trait::async_trait]
impl Handler for PinnedClientHandler {
    type Error = russh::Error;

    /// **핵심**: 이 콜백이 host key 수용 여부를 결정한다. 기본값에 의존하거나 무조건 Ok(true)를
    /// 반환하면 accept-any = MITM(H6). 반드시 지문 핀 검증(R1). [확인 필요] 0.62 정확 시그니처.
    async fn check_server_key(&mut self, server_public_key: &PublicKey)
        -> Result<bool, Self::Error>
    {
        match self.policy.verify(server_public_key) {
            Ok(()) => Ok(true),                 // 일치 → 수용
            Err(e) => {
                // 요란히 로깅 후 거부. true를 절대 반환하지 않음.
                tracing::error!("{e}");
                Ok(false)                        // false = russh가 연결을 끊음(Error::UnknownKey 계열)
            }
        }
    }
}
```

> russh 0.62의 `check_server_key`는 `Result<bool, Error>`를 반환하며 **`false`면 연결이 거부**된다
> (`Ok(true)`만 수용). 정확한 인자/반환 형태는 minor마다 흔들리므로 빌드 시 실측(`[확인 필요]`, R13).
> **테스트로 강제**: 바뀐 키에 대해 이 핸들러가 반드시 `false`를 내는지 T2로 증명한다.

### 3. 연결 + 인증 (R7)

```rust
use std::sync::Arc;
use russh::client::Config;

pub enum SshAuth {
    /// 개인키 파일 + (선택) passphrase(S02가 unwrap). 에이전트 경로는 옵션.
    PublicKey { identity_pem: Vec<u8>, passphrase: Option<secrecy::SecretString> },
    /// password(S02가 unwrap). enc:로만 저장(R7/R12).
    Password { password: secrecy::SecretString },
}

/// 바스티온이든 SFTP 서버든 공통 접속·인증 경로. Handle을 돌려준다.
pub async fn ssh_connect(
    host: &str, port: u16, user: &str,
    policy: HostKeyPolicy, auth: SshAuth,
) -> anyhow::Result<client::Handle<PinnedClientHandler>> {
    let config = Arc::new(Config::default());
    let handler = PinnedClientHandler::new(policy);

    // connect가 host key 검증(check_server_key)을 KEX 중 호출한다. 불일치면 여기서 실패.
    let mut handle = client::connect(config, (host, port), handler).await?;

    let authed = match auth {
        SshAuth::PublicKey { identity_pem, passphrase } => {
            let key = russh::keys::decode_secret_key(
                std::str::from_utf8(&identity_pem)?,
                passphrase.as_ref().map(|s| secrecy::ExposeSecret::expose_secret(s).as_str()),
            )?;   // [확인 필요] 0.62 개인키 로드 헬퍼명(decode_secret_key / PrivateKeyWithHashAlg)
            let key = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None);
            handle.authenticate_publickey(user, key).await?
        }
        SshAuth::Password { password } => {
            handle.authenticate_password(
                user, secrecy::ExposeSecret::expose_secret(&password),
            ).await?
        }
    };
    // 인증 성공 여부는 반환 타입으로 판정([확인 필요] 0.62: AuthResult/bool). 실패면 hard fail.
    anyhow::ensure!(authed.success(), "SSH 인증 실패: {host}:{port} user={user}");
    Ok(handle)
    // 주의(R12): password/passphrase/개인키 바이트를 로그에 절대 넣지 않는다.
}
```

### 4. Tunnel — direct-tcpip → 로컬 TcpListener 브리지 (R3/R4)

```rust
use tokio::net::{TcpListener, TcpStream};
use tokio::io::copy_bidirectional;

pub struct TunnelHandle {
    pub local_addr: std::net::SocketAddr,   // BackupSource가 접속할 127.0.0.1:<port>
    _accept_task: tokio::task::JoinHandle<()>,
    _keepalive: Arc<client::Handle<PinnedClientHandler>>,  // drop 시 세션 종료
}

/// 바스티온에 붙어 direct-tcpip 채널로 DB(host:port)에 닿게 하고, 로컬 소켓으로 노출한다.
/// pg_dump/mysqldump는 local_addr로 접속하면 된다(마치 DB가 로컬인 것처럼).
pub async fn open_tunnel(
    handle: client::Handle<PinnedClientHandler>,
    db_host: String, db_port: u16,
    local_bind: &str,               // "127.0.0.1:0" = OS가 포트 할당
) -> anyhow::Result<TunnelHandle> {
    let listener = TcpListener::bind(local_bind).await?;
    let local_addr = listener.local_addr()?;
    let handle = Arc::new(handle);
    let h = handle.clone();

    let accept_task = tokio::spawn(async move {
        loop {
            let (mut local_sock, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => { tracing::warn!("tunnel accept 실패: {e}"); break; }
            };
            let h2 = h.clone();
            let (dbh, dbp) = (db_host.clone(), db_port);
            tokio::spawn(async move {
                // 매 로컬 연결마다 새 direct-tcpip 채널을 연다.
                // 인자: (target_host, target_port, originator_host, originator_port). [확인 필요] 0.62 시그니처.
                let channel = match h2.channel_open_direct_tcpip(
                    dbh, dbp as u32, peer.ip().to_string(), peer.port() as u32,
                ).await {
                    Ok(c) => c,
                    Err(e) => { tracing::warn!("direct-tcpip open 실패: {e}"); return; }
                };
                let mut chan_stream = channel.into_stream();   // AsyncRead+AsyncWrite
                // 양방향 복사: 로컬 소켓 ↔ SSH 채널. 한쪽이 닫히면 종료(R4).
                if let Err(e) = copy_bidirectional(&mut local_sock, &mut chan_stream).await {
                    tracing::debug!("tunnel copy 종료: {e}");
                }
            });
        }
    });

    tracing::info!(local = %local_addr, target = %format!("{db_host}:{db_port}"), "터널 개통");
    Ok(TunnelHandle { local_addr, _accept_task: accept_task, _keepalive: handle })
}
```

> **run 시퀀스 정합(S05)**: SshDest가 아니라 **BackupSource가 바스티온 뒤 DB를 쓸 때** open_tunnel을
> dump보다 **먼저** 호출하고, S03/S04는 `TunnelHandle.local_addr`를 DB host:port로 사용한다. TunnelHandle을
> drop하면 accept 루프와 세션이 정리된다(백업 종료 후).

### 5. SshDest — S06 Destination 구현 (R5/R6/R8)

```rust
use russh_sftp::client::SftpSession;
use crate::dest::{Destination, DestCaps, DestWriter, ImmutabilityKind, SelfTestReport, DestError};

pub struct SshDest {
    name: String,
    host: String, port: u16, user: String,
    policy: HostKeyPolicy,
    auth_factory: AuthFactory,   // S02로부터 매 연결 시 auth 자료를 만든다(비영속)
    remote_dir: String,
    append_only: bool,           // 서버측 append-only가 외부 보장될 때만 true(R8)
}

#[async_trait::async_trait]
impl Destination for SshDest {
    fn name(&self) -> &str { &self.name }

    fn capabilities(&self) -> DestCaps {
        DestCaps {
            // SshDest는 Object Lock을 강제 못 함(R8/R9). 서버 append-only가 외부 보장이면
            // ServerAppendOnly로 자가주장, 아니면 None. 어느 쪽도 Governance/Compliance 아님.
            immutability: if self.append_only {
                ImmutabilityKind::ServerAppendOnly
            } else {
                ImmutabilityKind::None
            },
            checksum_required: false,   // SFTP엔 Object-Lock-checksum 요구 없음
            resumable: true,            // 스풀 재읽기 + 임시키 재업로드 + rename
        }
    }

    /// SFTP 업로드. multipart 없음 → **임시 key에 write 후 성공 시 rename**으로 커밋 원자성 확보(R6).
    async fn writer(&self, key: &str) -> Result<DestWriter, DestError> {
        let sftp = self.open_sftp().await.map_err(DestError::from_ssh)?;
        let final_path = format!("{}/{}", self.remote_dir.trim_end_matches('/'), key);
        let tmp_path = format!("{final_path}.part-{}", uuid_v4());
        // 원격 디렉터리 보장(mkdir -p 동등) 후 임시 파일 생성.
        ensure_remote_dir(&sftp, &self.remote_dir).await.map_err(DestError::from_ssh)?;
        let file = sftp.create(&tmp_path).await.map_err(DestError::from_ssh)?;
        Ok(DestWriter::sftp(SftpUpload {
            sftp, file, tmp_path, final_path, committed: false,
        }))
    }

    async fn self_test(&self) -> Result<SelfTestReport, DestError> {
        // 연결/인증/host-key 핀/원격 디렉터리 write 권한을 확인. WORM 자가주장은 검증 불가이므로
        // append_only=true여도 "외부 보장 필요" 경고를 남긴다(오인 방지, R9).
        let sftp = self.open_sftp().await.map_err(DestError::from_ssh)?;
        let probe = format!("{}/.selftest-{}", self.remote_dir.trim_end_matches('/'), uuid_v4());
        let mut f = sftp.create(&probe).await.map_err(DestError::from_ssh)?;
        f.write_all(b"ok").await.map_err(DestError::from_ssh)?;
        f.flush().await.map_err(DestError::from_ssh)?;
        let _ = sftp.remove_file(&probe).await;   // probe 정리(writer 신원이 지울 수 있음 — WORM 아님)
        Ok(SelfTestReport::ssh_writable(&self.name, self.append_only))
    }
}
```

> **rename-commit이 S06 multipart-only-commit과 정합하는 이유(R6)**: SFTP엔 CompleteMultipartUpload가
> 없다. 대신 `.part-<uuid>` 임시 경로에 **전량 write + flush**한 뒤, **성공했을 때만** 최종 key로
> `rename`한다. 실패 시 임시 파일을 지운다. 그래서 **최종 key에는 절단 객체가 절대 나타나지 않는다**
> (부분 성공 없음). S06 `DestWriter`의 `commit()`/`abort()` 의미론을 SFTP 백엔드로 그대로 구현한 것.

### 6. SftpUpload — write/flush/rename-commit/abort (R6)

```rust
/// S06 DestWriter가 감싸는 SFTP 업로드 상태. commit=rename, abort=임시파일 삭제.
pub struct SftpUpload {
    sftp: SftpSession,
    file: russh_sftp::client::fs::File,
    tmp_path: String,
    final_path: String,
    committed: bool,
}

impl SftpUpload {
    pub async fn write_all(&mut self, buf: bytes::Bytes) -> Result<(), DestError> {
        use tokio::io::AsyncWriteExt;
        self.file.write_all(&buf).await.map_err(DestError::from_ssh)
    }

    /// 완전 성공 경로에서만 호출: flush → 파일 닫기 → 임시→최종 rename(= 유일한 커밋 지점).
    pub async fn commit(mut self) -> Result<UploadReceipt, DestError> {
        use tokio::io::AsyncWriteExt;
        self.file.flush().await.map_err(DestError::from_ssh)?;
        self.file.shutdown().await.ok();                 // 핸들 닫기(서버가 파일 확정)
        self.sftp.rename(&self.tmp_path, &self.final_path)   // 원자적 커밋
            .await.map_err(DestError::from_ssh)?;
        self.committed = true;
        Ok(UploadReceipt { key: self.final_path.clone(), checksum: None /* 로컬 재계산은 S08 */ })
    }

    /// 실패 경로: 임시 파일 삭제 → 최종 key엔 아무것도 나타나지 않음.
    pub async fn abort(mut self) {
        use tokio::io::AsyncWriteExt;
        let _ = self.file.shutdown().await;
        if let Err(e) = self.sftp.remove_file(&self.tmp_path).await {
            tracing::warn!(path = %self.tmp_path, "임시 SFTP 파일 삭제 실패(수동 정리 필요): {e}");
        }
    }
}
```

> `open_with_flags(final, CREATE|WRITE|EXCLUDE)`로 **최종 경로를 곧장** 여는 변형도 가능하나, 부분 write가
> 최종 key에 노출될 수 있어 채택하지 않는다. **임시키+rename**이 절단 노출 0을 보장한다(R6).

### 7. SshDest의 SFTP 세션 열기 (R5)

```rust
impl SshDest {
    /// 매 업로드마다 새 SSH 연결 → session 채널 → sftp 서브시스템 → SftpSession.
    async fn open_sftp(&self) -> anyhow::Result<SftpSession> {
        let auth = self.auth_factory.make().await?;    // S02 unwrap(비영속, R7)
        let handle = ssh_connect(&self.host, self.port, &self.user, self.policy.clone(), auth).await?;
        let channel = handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;                 // SFTP 서브시스템 활성
        let sftp = SftpSession::new(channel.into_stream()).await?;      // russh-sftp v3 클라이언트
        Ok(sftp)
    }
}
```

### 8. Job 불변성 assert — SshDest는 Governance 요구를 만족 못 함 (R9)

```rust
// S06 assert_job_immutability를 그대로 사용. SshDest.capabilities().immutability는
// None 또는 ServerAppendOnly이므로:
//   - RequiredImmutability::None           → 통과
//   - RequiredImmutability::AppendOnlyOrLock→ append_only=true(ServerAppendOnly)일 때만 통과
//   - RequiredImmutability::ObjectLock      → **항상 실패**(SshDest는 Object Lock 불가)
//   - RequiredImmutability::ComplianceOnly  → **항상 실패**
//
// 즉 "Governance/Compliance를 assert하는 Job이 SFTP만 가리키면 요란히 실패"가 자동으로 성립(P1-2).
// 랜섬웨어 방어가 필요한 Job은 반드시 S3Dest(Object Lock)를 포함해야 한다.
```

## Test Scenarios

| # | 시나리오 | 기대 | 검증 포인트 |
|---|---------|------|------------|
| T1 | **올바른 host key**: config 핀 == 서버 실제 지문 | `check_server_key`가 `Ok(true)`, 연결 성립 | R1 정상 경로 |
| T2 | **바뀐 host key(MITM 모사)**: 서버 키를 바꿔 재접속 | `check_server_key`가 `false` → 연결 **거부**, `HostKeyError::Changed` 로깅 | **R1/R11/H6 핵심** |
| T3 | 미지 host key(핀과 불일치) | 연결 거부(정책상 Changed와 동일 hard fail) | R1/R11 |
| T4 | 지문 형식: `ssh-keygen -lf`의 `SHA256:...`와 우리 계산 값 일치 | 두 방식 문자열 동일 | R2 |
| T5 | **터널 개통 → 로컬 소켓으로 DB 접속** | `local_addr`로 TCP 접속 시 바스티온 뒤 DB에 도달, pg_dump 성공 | R3/R4 |
| T6 | 터널 다중 동시 연결 | 각 연결이 독립 direct-tcpip 채널로 브리지 | R4 |
| T7 | 터널 대상(DB) 다운 | direct-tcpip open 실패가 로컬 연결로 전파(dump 실패) | R4 |
| T8 | **SshDest 업로드 happy path** | 임시키 write→flush→rename, 최종 key에 온전한 파일 | R5/R6 |
| T9 | **업로드 중 실패 → abort** | 임시키 삭제, **최종 key엔 아무 파일 없음**(절단 노출 0) | R6/P0-3 |
| T10 | SshDest caps(append_only=false) | immutability=None, resumable=true | R8 |
| T11 | SshDest caps(append_only=true) | immutability=ServerAppendOnly | R8 |
| T12 | **Job이 ObjectLock 요구 + SshDest만** | `assert_job_immutability`가 `ImmutabilityUnmet`로 실패 | **R9/P1-2 핵심** |
| T13 | Job이 ObjectLock 요구 + SshDest+S3(Lock) | 통과(락 목적지 ≥1) | R9 |
| T14 | 공개키 인증(passphrase enc:) / password 인증(enc:) | S02 unwrap 후 인증 성공, 시크릿 미로깅 | R7/R12 |
| T15 | 로그 스크럽: 인증 실패 로그에 password/passphrase 없음 | 로그에 host/port/user만, 시크릿 0 | R12/M6 |

> 하네스: 로컬 **OpenSSH sshd 컨테이너**(또는 러shr 서버)로 결정적 재현. T2/T3는 서버 host key를
> 재생성해 “바뀐 키”를 모사하고, 핸들러가 반드시 거부하는지 단언. T5는 sshd + 뒤단 postgres 컨테이너
> 2단으로 바스티온 경유 접속을 재현. T9는 write 도중 강제 에러 주입 후 원격 디렉터리에 최종 key 부재를 단언.

## Implementation Strategy

1. **host-key 핀 먼저(T1~T4)**: `HostKeyPolicy` + `PinnedClientHandler`를 세우고, 바뀐 키 거부(T2)를
   가장 먼저 통과시킨다 — 이 섹션의 보안 하중이자 H6/P2-1의 핵심. 지문 형식(R2)을 `ssh-keygen`과 대조.
2. **연결·인증(T14/T15)**: `ssh_connect`(공개키/password) + S02 unwrap + 로그 스크럽. 시크릿이 로그·에러에
   새지 않는지(M6) 함께 고정.
3. **Tunnel(T5~T7)**: `open_tunnel`(direct-tcpip → TcpListener 브리지). 2단 컨테이너로 바스티온 경유
   DB 접속을 실증. `channel_open_direct_tcpip`/`into_stream` 0.62 시그니처 실측(`[확인 필요]`, R13).
4. **SshDest 업로드(T8~T11)**: S06 `Destination` 구현 + rename-commit(R6). abort 시 최종 key 부재(T9)를
   증명 — S06 multipart-only-commit 규율의 SFTP판.
5. **Job assert(T12/T13)**: S06 `assert_job_immutability`로 Governance 요구가 SFTP만으로는 실패함을 고정
   (P1-2). SshDest를 랜섬웨어 방어 목적지로 오인하지 못하게 한다.
6. **버전 핀·공급망(H6)**: russh/russh-sftp `=` 핀 + `Cargo.lock` 커밋 + cargo-deny(S11). API churn 대비
   `[확인 필요]` 지점(시그니처)을 빌드 시 일괄 해소.

## Quality Gate

- `cargo test` — T1~T15 통과. 특히 **T2(바뀐 키 거부)**, **T9(abort 시 최종 key 부재)**,
  **T12(Governance 요구 SFTP 실패)**.
- `cargo clippy -- -D warnings`, `cargo fmt --check`.
- **불변식**:
  (a) `check_server_key`는 **핀 정확 일치일 때만** `Ok(true)` — 미지/불일치는 어떤 경로로도 수용되지 않는다
  (accept-any/TOFU 부재를 테스트로 증명, H6).
  (b) 어떤 업로드 실패 경로에서도 **최종 key에 부분 파일이 나타나지 않는다**(rename-commit, R6).
  (c) SshDest는 `Governance/Compliance` 캡을 **절대 보고하지 않는다** — Object Lock 요구 Job은 반드시 실패
  (R9).
  (d) password/passphrase/개인키 바이트가 **로그·에러 메시지에 등장하지 않는다**(R12/M6).
- `cargo deny check` — russh/russh-sftp 핀·라이선스·advisory 게이트, openssl 유입 0(H6).

## Risk & Rollback

| 리스크 | 영향 | 완화 |
|--------|------|------|
| **russh 0.62 API churn**(R10/R13) | `check_server_key`/`channel_open_direct_tcpip`/`authenticate_*` 시그니처 불일치로 빌드 실패 | `=0.62.2` 정확 핀 + `Cargo.lock`. `[확인 필요]` 지점 빌드 시 실측, 컴파일 에러가 안전망 |
| **check_server_key를 구현 안 함/오구현** | accept-any = **MITM**(H6) | 정책층 단일화(`HostKeyPolicy::verify`) + T2/T3로 거부 강제. 기본값 의존 금지 |
| **russh-sftp ↔ russh 0.62 버전 비호환** | into_stream 타입 불일치로 SFTP 불가 | 호환 조합 실측(`[확인 필요]`), 대안: SFTP 미지원 시 `exec`로 `cat > file` 스트리밍(차선) 또는 ssh2 fallback(단 Windows 빌드 마찰↑) |
| **SFTP rename 비원자/미지원 서버** | 커밋 원자성 깨짐 | 대다수 서버 rename 원자적. 미지원 시 임시키 유지 + 매니페스트가 최종 진실(S08)로 판정, 고아 `.part` 주기 정리 |
| **터널 세션 조기 종료**(keepalive 없음) | 장시간 dump 중 채널 끊김 | `TunnelHandle`이 `Handle` 소유(생명주기 고정) + keepalive 인터벌 설정 검토 `[확인 필요]` |
| **개인키 passphrase 로그 유출**(M6) | 시크릿 노출 | `secrecy::SecretString`로만 취급, Debug 미구현 자료, 에러 메시지 스크럽(R12) |
| **SshDest를 WORM으로 오인** | 랜섬웨어에 취약한 목적지를 방어선으로 착각 | caps=None/ServerAppendOnly 고정, `assert_job_immutability`가 Governance 요구를 실패시킴(R9/T12) |

**Rollback**: S07은 라이브러리 계층. Tunnel은 부작용이 로컬 소켓 + 아웃바운드 SSH뿐이라 TunnelHandle drop으로
원자적 정리(no-op 회귀). SshDest 업로드 실패는 임시키 삭제 + 최종 key 미노출로 복구. 기능 플래그로 SshDest를
끄면 슬라이스 2 상태(Local+S3)로 회귀하되, SSH를 요구하던 Job은 목적지 부재로 요란히 실패(오인 방지). host-key
정책이 거부하면 **애초에 연결/업로드를 시작하지 않으므로** 원자적으로 안전.

## Acceptance Criteria

- [ ] `client::Handler::check_server_key`가 config `host_key_fpr`(SHA256)와 **정확 일치 시에만** 수용,
      미지/불일치는 거부(R1/R2).
- [ ] 바뀐 host key는 `HostKeyError::Changed`로 **MITM 경고**하며 거부(R11) — T2 통과.
- [ ] accept-any/TOFU 경로가 코드·기본값 어디에도 없음(H6) — 테스트로 증명.
- [ ] Tunnel: 바스티온 접속 → `channel_open_direct_tcpip` → 로컬 `TcpListener` 브리지, BackupSource가
      `local_addr`로 DB 접속(R3/R4) — T5 통과.
- [ ] 터널 다중 연결·대상 다운 전파(R4) — T6/T7.
- [ ] SshDest가 S06 `Destination` 구현: `writer`(SFTP create+스트리밍), `capabilities`, `self_test`(R5).
- [ ] 업로드는 **임시키 write→flush→rename-commit**, 실패 시 abort로 최종 key 미노출(R6) — T8/T9.
- [ ] 인증: 공개키(passphrase `enc:`) 또는 password(`enc:`), S02 unwrap, 시크릿 미로깅(R7/R12) — T14/T15.
- [ ] `DestCaps.immutability` = None 또는 ServerAppendOnly, checksum_required=false, resumable=true(R8).
- [ ] **Governance/Compliance 요구 Job은 SshDest에서 실패**(assert_job_immutability, R9) — T12/T13.
- [ ] russh/russh-sftp `=` 정확 버전 핀 + `Cargo.lock` 커밋 + cargo-deny(R10/H6).
- [ ] russh 0.62 정확 시그니처 `[확인 필요]` 지점을 빌드 시 실측·해소(R13).
- [ ] T1~T15 전부 통과.

## Files

| 파일 | 내용 |
|------|------|
| `src/ssh/mod.rs` | 공개 API 재수출, `SshError`(시크릿 스크럽 오류 매핑) |
| `src/ssh/hostkey.rs` | `HostKeyPolicy`(SHA256 핀 verify), `HostKeyError`(Unknown/Changed, R11) |
| `src/ssh/handler.rs` | `PinnedClientHandler`(`client::Handler`, `check_server_key` — accept-any 금지) |
| `src/ssh/connect.rs` | `ssh_connect`(연결+인증), `SshAuth`(공개키/password), S02 unwrap 연동 |
| `src/ssh/tunnel.rs` | `open_tunnel`/`TunnelHandle`(direct-tcpip → TcpListener 브리지, copy_bidirectional) |
| `src/dest/ssh.rs` | `SshDest`(S06 `Destination` 구현), `capabilities`(None/ServerAppendOnly), `self_test` |
| `src/dest/ssh_upload.rs` | `SftpUpload`(write/flush/rename-commit/abort), `open_sftp`(request_subsystem "sftp") |
| `tests/ssh_hostkey.rs` | T1~T4(핀 일치/바뀜/미지 거부/지문 형식) — sshd 컨테이너 하네스 |
| `tests/ssh_tunnel.rs` | T5~T7(터널 개통/다중/대상 다운) — sshd+postgres 2단 하네스 |
| `tests/ssh_dest.rs` | T8~T15(업로드/abort/caps/Job assert/인증/로그 스크럽) |
| `tests/support/sshd.rs` | OpenSSH sshd 컨테이너 하네스(host key 재생성 유틸 포함, T2용) |

---
`#tags: sshdest, russh, host-key-pin, direct-tcpip, tunnel, sftp, mitm, destination-trait, rename-commit, append-only, rust, section-s07`
