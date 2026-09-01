# S05 — Pipeline + Commit Protocol (옵션 C 스풀-후-팬아웃)

> 의존: S01(스캐폴딩/Config/CLI), S02(SecretManager — 백업별 DEK/recipient 제공)
> 레이어: 2 · 슬라이스 1의 하중 리스크(finish/절단, 평문해시 tee, 옵션 C)를 조기 증명하는 핵심 섹션.

## Background

이 섹션은 한 번의 논리 덤프를 **압축 → 암호화 → 로컬 스풀**로 흘려보내고(단일 통과), 그 스풀 파일을
목적지별로 독립 업로드(팬아웃)하는 파이프라인을 구현한다. 도메인 사전의 두 규범 용어가 이 섹션의 뼈대다.

- **Spool-once fan-out(옵션 C)**: `1회 덤프 → zstd → age → 로컬 스풀(=LocalDest) → 목적지별 독립 업로드`.
  대안 B(in-pipe tee: 파이프 도중 여러 목적지로 동시 분기)는 가장 느린 업로드가 DB 스냅샷/트랜잭션을
  붙잡아 원본 DB에 bloat를 유발한다(팀리뷰 P0-4). 옵션 C는 **스풀 write가 끝나는 즉시 DB 스냅샷을
  해제**하고, 이후 느린 네트워크 전송은 DB와 무관하게 진행된다.
- **Commit Protocol**: `exit0 + finish(inner→outer) + flush/fsync + (매니페스트) 후에만 커밋`.
  절단된 덤프가 "유효한 백업"으로 커밋되는 것이 이 시스템 최악의 실패다(복원 시점에야 발각). 이를 막는
  네 개의 규율이 팀리뷰 P0-1/2/3에 대응한다.

핵심 제약 세 가지를 먼저 못박는다.

1. **age/zstd는 동기(`std::io::Write`) API**이고 덤프 stdout은 tokio(`AsyncRead`)다. 이 경계를
   `tokio_util::io::SyncIoBridge`로 잇고, 동기 인코딩 체인 전체를 `spawn_blocking` 안에서 돌린다.
   이렇게 해야 `finish()`가 결정적(동기·순서보장)으로 실행된다.
2. **Rust `Drop`은 `finish()`를 대신할 수 없다**(팀리뷰 P0-2). age `StreamWriter`는 drop되어도 마지막
   인증 태그를 쓰지 않는다 → 조용히 절단된 복호 불가 파일이 남는다. 따라서 finish는 **명시적**이어야 하고,
   drop에 의존하면 안 된다. debug 빌드에서 finish 없이 drop되면 panic하는 가드를 둔다.
3. **평문은 암호화 스풀 밖으로 새면 안 된다**(팀리뷰 M2). Stream 덤프는 디스크에 평문을 쓰지 않는다.
   Directory 덤프(mydumper/`pg_dump -Fd`)만 TempDir에 평문을 잠깐 두므로, tar로 체인에 말아 넣은 직후
   즉시 안전 삭제한다(§Risk).

## Requirements

| # | 요구 | 근거 |
|---|------|------|
| R1 | 덤프 → blake3 tee(평문 해시, 스트리밍) → zstd → age(백업별 DEK) → 로컬 스풀 단일 통과 | plan §파이프라인 4, P1-3 |
| R2 | 자식 프로세스 exit code 확인 — **EOF에 도달했더라도 exit≠0이면 FAIL** | P0-1 |
| R3 | finish 순서 **inner→outer**: `zstd.finish()` → `age.finish()` → `flush`+`fsync` | P0-2 |
| R4 | "스트림 복사 완료 ~ 커밋 확정" 구간에서 **`?` 조기 반환 금지** | P0-3 |
| R5 | debug 전용 "finished?" 가드 — finish 없이 drop되면 panic | P0-2 |
| R6 | 커밋 후 목적지별 **독립 업로드**(JoinSet), 목적지별 retry/resume, **부분 성공 벡터** 반환 | P0-4 옵션 C |
| R7 | 스풀 write 완료 즉시 DB 스냅샷 해제(팬아웃은 DB와 분리) | P0-4 |
| R8 | 병렬 덤프 수를 세마포어로 상한(`max_parallel_dumps`) | plan Open Q |
| R9 | 사전 디스크 여유 가드(`min_free_disk`) — 스풀 볼륨 부족 시 시작 거부 | 절단/부분쓰기 예방 |
| R10 | 암호화 스풀 외 평문 임시 파일 spill 없음. Directory 아티팩트는 동일 체인에 tar-stream | M2, P1-1 |
| R11 | `DumpArtifact = Stream(AsyncRead) \| Directory(TempDir)` 둘 다 동일 인코딩 체인 통과 | P1-1 |

## Dependencies

- **입력(S02 SecretManager)**: 백업별 DEK로부터 유도된 `age` recipient(들). S05는 DEK 원본을 만지지
  않는다 — payload 암호화(age)와 DEK wrap/unwrap을 분리한다(P0-5).
- **입력(S03/S04 BackupSource)**: `spawn(part) -> DumpArtifact`. Stream/Directory와 자식 종료 대기
  핸들을 제공한다. S05는 소비자(consumer)다.
- **출력 소비(S06/S07 Destination)**: 커밋된 스풀 파일 경로를 받아 목적지별로 업로드. 팬아웃 orchestration은
  S05, 실제 write/multipart/resume는 Destination.
- **출력 소비(S08 Catalog/Manifest)**: 평문 blake3 해시·바이트 수·스풀 경로를 매니페스트에 기록.

## Reference Libraries

| 크레이트 | 버전 | 이 섹션에서의 역할 | Gotcha |
|---------|------|------|--------|
| `zstd` | 0.13.3 | `stream::write::Encoder<W>` 스트리밍 압축(level 3 기본) | `finish()` 필수 — 안 하면 프레임 절단 |
| `age` | 0.12.1 | `Encryptor::with_recipients(..).wrap_output(W)` → `StreamWriter`(STREAM/ChaCha20-Poly1305) | `StreamWriter::finish()` 필수 — drop만으론 태그 미기록 → 복호 불가. BETA(포맷 v1은 안정) |
| `blake3` | 1.x | `Hasher::update`로 평문 스트리밍 해시, `finalize()` → 32B | 반드시 **평문**(zstd 이전)에 tee. 실제 write된 바이트만 update |
| `tokio` | 1.x | `spawn_blocking`(동기 체인), `process`, `sync::Semaphore`, **`task::JoinSet`**(팬아웃) | JoinSet은 `tokio::task`에 있음(futures 아님). 명세의 "futures JoinSet"은 tokio 것으로 구현 |
| `tokio-util` | 0.7.x | `io::SyncIoBridge` — `AsyncRead`를 `std::io::Read`로 브리지 | `spawn_blocking` 안에서만. 런타임 핸들 필요 |
| `tar` | 0.4.x | Directory 아티팩트를 체인에 push(tar-stream) | `Builder::finish()` 필수(트레일러) |
| `tempfile` | 3.x | Directory 덤프의 평문 TempDir | tar 직후 안전 삭제(M2) |
| `fs2` | 0.4.x | `available_space(path)` 사전 디스크 가드 | Windows는 `GetDiskFreeSpaceEx` 경유 |

`Cargo.toml` (발췌):
```toml
[dependencies]
zstd = "=0.13.3"
age = { version = "=0.12.1", features = ["armor"] }   # recipient 모드; armor는 선택
blake3 = "1"
tokio = { version = "1", features = ["rt-multi-thread","process","io-util","macros","sync","fs"] }
tokio-util = { version = "0.7", features = ["io"] }    # SyncIoBridge
tar = "0.4"
tempfile = "3"
fs2 = "0.4"
anyhow = "1"
thiserror = "2"
tracing = "0.1"
```
> security-critical 크레이트(`age`,`zstd`)는 `=x.y.z` 핀(팀리뷰 H6). `age` 0.12 recipient 시그니처는
> 빌드 시 확인 `[확인 필요]`.

## Implementation

### 1. 타입 정의

```rust
/// BackupSource(S03/S04)가 넘겨주는 덤프 출력. 둘 다 동일 인코딩 체인을 통과한다.
pub enum DumpArtifact {
    /// pg_dump -Fc -f - / mysqldump ... → stdout 파이프
    Stream(DumpStream),
    /// mydumper / pg_dump -Fd → 디렉토리(TempDir), tar로 말아 체인 투입
    Directory(DumpDir),
}

pub struct DumpStream {
    pub reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    /// 자식 종료를 대기 → exit status + stderr tail. 스트림 EOF와 별개로 확인(R2).
    pub wait: futures::future::BoxFuture<'static, anyhow::Result<DumpExit>>,
}
pub struct DumpDir {
    pub root: tempfile::TempDir,   // 평문 — tar 직후 안전 삭제(M2)
    pub wait: futures::future::BoxFuture<'static, anyhow::Result<DumpExit>>,
}
pub struct DumpExit {
    pub code: Option<i32>,
    pub success: bool,
    pub stderr_tail: String,       // 진단/치명 패턴 검사용
}

/// 커밋된 스풀의 진실 소스 필드(→ S08 Manifest).
pub struct SpoolCommit {
    pub spool_path: std::path::PathBuf,   // 이 파일이 곧 LocalDest 산출물
    pub plaintext_blake3: [u8; 32],       // 평문 해시(P1-3)
    pub plaintext_bytes: u64,
    pub ciphertext_bytes: u64,
    pub stderr_tail: String,
}
```

### 2. blake3 tee 라이터 (평문 스트리밍 해시)

zstd **이전**에 위치해 압축 전 평문을 해시하고, 그대로 아래로 흘려보낸다.

```rust
struct HashingWriter<W: std::io::Write> {
    inner: W,
    hasher: blake3::Hasher,
    bytes: u64,
}
impl<W: std::io::Write> HashingWriter<W> {
    fn new(inner: W) -> Self { Self { inner, hasher: blake3::Hasher::new(), bytes: 0 } }
    /// 해시/카운트를 뽑고 내부 라이터를 반환(finish 순서에서 zstd로 위임).
    fn finalize(self) -> ([u8; 32], u64, W) {
        (*self.hasher.finalize().as_bytes(), self.bytes, self.inner)
    }
}
impl<W: std::io::Write> std::io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;      // 부분 write 대비: 실제 쓰인 n만 해시
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}
```

### 3. finish 가드 (P0-2/R5)

```rust
/// debug에서 finish 없이 drop되면 panic — 절단 스풀 조기 검출.
struct FinishGuard { finished: bool }
impl FinishGuard {
    fn new() -> Self { Self { finished: false } }
    fn mark(&mut self) { self.finished = true; }
}
impl Drop for FinishGuard {
    fn drop(&mut self) {
        debug_assert!(
            self.finished,
            "pipeline dropped without finish() — 절단 백업 위험(P0-2)"
        );
    }
}
```

### 4. 인코딩 체인 (sink 바깥에서 안쪽으로 조립)

쓰기 방향: `평문 → HashingWriter(tee) → zstd → age → BufWriter<File>(스풀)`.
조립 방향은 반대(파일부터). 타입 별칭으로 가독성 확보.

```rust
type Spool  = std::io::BufWriter<std::fs::File>;
type AgeW   = age::stream::StreamWriter<Spool>;
type ZstdW  = zstd::stream::write::Encoder<'static, AgeW>;
type ChainW = HashingWriter<ZstdW>;

struct EncryptChain {
    tee: ChainW,
    spool_path: std::path::PathBuf,
    guard: FinishGuard,
}

impl EncryptChain {
    fn new(
        spool_path: std::path::PathBuf,
        recipients: Vec<Box<dyn age::Recipient + Send + 'static>>,  // S02 백업별 DEK 유도
        zstd_level: i32,
    ) -> anyhow::Result<Self> {
        // 조립은 스트림 복사 이전이라 `?` 허용(아직 커밋 구간 아님).
        let file = std::fs::File::create(&spool_path)?;
        let spool: Spool = std::io::BufWriter::new(file);
        let encryptor = age::Encryptor::with_recipients(recipients)   // [확인 필요] 0.12 시그니처
            .ok_or_else(|| anyhow::anyhow!("recipient가 비어있음"))?;
        let age_w: AgeW = encryptor.wrap_output(spool)?;              // age STREAM writer
        let zstd_w: ZstdW = zstd::stream::write::Encoder::new(age_w, zstd_level)?;
        Ok(Self { tee: HashingWriter::new(zstd_w), spool_path, guard: FinishGuard::new() })
    }

    /// 복사(pull) / tar(push) 양쪽이 여기로 쓴다.
    fn writer(&mut self) -> &mut ChainW { &mut self.tee }

    /// finish inner→outer + flush + fsync. R3/R4 준수 — 이 함수 안에서 `?` 조기반환하지 않고
    /// 실패는 전부 Err로 수렴시켜 호출부가 스풀을 삭제하게 한다.
    fn commit(self) -> anyhow::Result<([u8; 32], u64, u64)> {
        let EncryptChain { tee, spool_path: _, mut guard } = self;
        let (hash, plain_bytes, zstd_w) = tee.finalize();   // 평문 해시 확정
        // ---- inner→outer, 명시적 finish (P0-2) ----
        let age_w  = zstd_w.finish()?;      // (1) zstd 프레임 마감
        let spool  = age_w.finish()?;       // (2) age 인증 태그 기록
        let mut file = spool.into_inner()   // BufWriter → File
            .map_err(|e| anyhow::anyhow!("스풀 flush 실패: {e}"))?;
        use std::io::Write as _;
        file.flush()?;
        file.sync_all()?;                   // (3) fsync — 전원 유실에도 스풀 온전
        let ciphertext_bytes = file.metadata()?.len();
        guard.mark();                       // 여기까지 오면 커밋 성공
        Ok((hash, plain_bytes, ciphertext_bytes))
    }
}
```

### 5. Commit Protocol (do/finally 형태 — Rust엔 finally가 없다)

Rust는 `try/finally`가 없으므로, **크리티컬 구간을 `?` 없는 블록으로 묶고** 결과를 단일 `match`로
수렴시켜 정리(스풀 삭제)를 보장한다. 이것이 P0-3의 "복사~커밋 사이 조기반환 금지"의 구체 형태다.

```rust
/// Stream 아티팩트를 인코딩·커밋. spawn_blocking 안에서 호출(동기 체인).
fn drive_stream_blocking(
    reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    spool_path: std::path::PathBuf,
    recipients: Vec<Box<dyn age::Recipient + Send + 'static>>,
    zstd_level: i32,
) -> anyhow::Result<([u8; 32], u64, u64)> {
    use tokio_util::io::SyncIoBridge;
    let mut src = SyncIoBridge::new(reader);              // AsyncRead → std::io::Read
    let mut chain = EncryptChain::new(spool_path.clone(), recipients, zstd_level)?;

    // ===== CRITICAL REGION: 스트림 복사 → 커밋. 이 구간엔 `?` 금지(P0-3). =====
    let copied: anyhow::Result<u64> = std::io::copy(&mut src, chain.writer())
        .map(|n| n)
        .map_err(anyhow::Error::from);

    let outcome = match copied {
        Ok(_) => chain.commit(),        // finish inner→outer(R3)
        Err(e) => { chain.abort_hint(); Err(e) }   // 복사 실패 → commit 자체를 시도 안 함
    };
    // ===== CRITICAL REGION END =====

    match outcome {
        Ok(v) => Ok(v),
        Err(e) => {
            // 절단/실패 스풀이 "유효한 백업"으로 남지 않게 삭제(핵심).
            let _ = std::fs::remove_file(&spool_path);
            Err(e)
        }
    }
}
```
> `abort_hint()`는 `EncryptChain`을 소비해 `guard.mark()` 후 drop(복사 실패 시 finish는 무의미하므로
> debug panic만 피하고 파일은 상위에서 삭제). 실제 삭제는 항상 호출부의 단일 정리 경로가 담당한다.

**exit code 확인(R2)** 은 복사/커밋과 별개로 `wait` 퓨처에서 회수한다. Stream은 EOF ≠ 성공이므로,
스풀을 커밋했더라도 자식이 exit≠0이면 스풀을 폐기한다.

```rust
async fn run_stream_pipeline(
    stream: DumpStream,
    spool_path: std::path::PathBuf,
    recipients: Vec<Box<dyn age::Recipient + Send + 'static>>,
    cfg: &PipelineCfg,
) -> anyhow::Result<SpoolCommit> {
    let sp = spool_path.clone();
    let level = cfg.zstd_level;
    let DumpStream { reader, wait } = stream;

    // 동기 인코딩 체인은 blocking 풀에서.
    let encode = tokio::task::spawn_blocking(move || {
        drive_stream_blocking(reader, sp, recipients, level)
    });

    // 자식 종료(+stderr) 대기와 인코딩을 함께 몰기.
    let (encode_res, exit) = tokio::join!(encode, wait);
    let (hash, plain, cipher) = encode_res??;   // spawn_blocking join + 내부 Result
    let exit = exit?;

    // R2: EOF에 도달했어도 exit≠0 또는 치명 stderr면 FAIL → 스풀 폐기.
    if !exit.success || is_fatal_stderr(&exit.stderr_tail, &cfg.fatal_patterns) {
        let _ = std::fs::remove_file(&spool_path);
        anyhow::bail!(
            "덤프 실패로 스풀 폐기: code={:?} stderr_tail={}",
            exit.code, exit.stderr_tail
        );
    }
    Ok(SpoolCommit {
        spool_path,
        plaintext_blake3: hash,
        plaintext_bytes: plain,
        ciphertext_bytes: cipher,
        stderr_tail: exit.stderr_tail,
    })
}
```

### 6. Directory 아티팩트 → tar-stream (R10/R11)

mydumper/`pg_dump -Fd`는 디렉토리를 만든다. 자식이 **먼저 완료**(dir 완성)되어야 하므로 exit 확인이
tar보다 앞선다. tar는 push 방식이라 체인의 `writer()`에 직접 append한다.

```rust
async fn run_directory_pipeline(
    dir: DumpDir,
    spool_path: std::path::PathBuf,
    recipients: Vec<Box<dyn age::Recipient + Send + 'static>>,
    cfg: &PipelineCfg,
) -> anyhow::Result<SpoolCommit> {
    // 1) 덤프 완료 및 exit 확인이 tar보다 먼저(R2).
    let exit = dir.wait.await?;
    if !exit.success || is_fatal_stderr(&exit.stderr_tail, &cfg.fatal_patterns) {
        anyhow::bail!("Directory 덤프 실패: code={:?}", exit.code);
    }
    let root = dir.root;   // TempDir(평문) — tar 직후 삭제
    let sp = spool_path.clone();
    let level = cfg.zstd_level;

    let (hash, plain, cipher) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mut chain = EncryptChain::new(sp.clone(), recipients, level)?;

        // ===== CRITICAL REGION: tar append → 커밋. `?`는 아래 블록으로 수렴 =====
        let tarred: anyhow::Result<()> = (|| {
            let mut builder = tar::Builder::new(chain.writer());  // &mut ChainW: Write
            builder.append_dir_all(".", root.path())?;
            builder.finish()?;      // tar 트레일러 — 반드시
            Ok(())                  // builder drop → chain 차용 종료
        })();

        let out = match tarred {
            Ok(()) => chain.commit(),
            Err(e) => { chain.abort_hint(); Err(e) }
        };
        // M2: 평문 TempDir 즉시 안전 삭제(tar가 체인에 다 들어간 뒤).
        secure_wipe_tempdir(root);
        match out {
            Ok(v) => Ok(v),
            Err(e) => { let _ = std::fs::remove_file(&sp); Err(e) }
        }
    }).await??;

    Ok(SpoolCommit {
        spool_path, plaintext_blake3: hash,
        plaintext_bytes: plain, ciphertext_bytes: cipher,
        stderr_tail: exit.stderr_tail,
    })
}
```

### 7. 세마포어 + 사전 디스크 가드 (R8/R9)

```rust
pub struct PipelineCfg {
    pub zstd_level: i32,                 // 기본 3
    pub max_parallel_dumps: usize,       // 세마포어 상한
    pub min_free_disk: u64,              // 스풀 볼륨 최소 여유(바이트)
    pub upload_retries: u32,
    pub fatal_patterns: Vec<String>,
}

/// 덤프 시작 전 스풀 볼륨 여유 확인 — 부족하면 절단/부분쓰기 전에 거부(R9).
fn preflight_disk_guard(spool_dir: &std::path::Path, min_free: u64) -> anyhow::Result<()> {
    let free = fs2::available_space(spool_dir)?;   // Windows: GetDiskFreeSpaceEx
    anyhow::ensure!(
        free >= min_free,
        "스풀 볼륨 여유 부족: free={free} < min_free_disk={min_free}"
    );
    Ok(())
}

// 잡 실행부(발췌): 세마포어로 병렬 덤프 상한.
async fn run_job(
    sem: std::sync::Arc<tokio::sync::Semaphore>,
    source: &dyn BackupSource,
    part: DumpPlanPart,
    cfg: &PipelineCfg,
    spool_dir: &std::path::Path,
) -> anyhow::Result<SpoolCommit> {
    preflight_disk_guard(spool_dir, cfg.min_free_disk)?;
    let _permit = sem.acquire().await?;            // R8: 동시 덤프 상한
    let recipients = /* S02: 백업별 DEK → recipients */;
    let spool_path = spool_dir.join(format!("{}.age", part.backup_id));
    match source.spawn(&part)? {
        DumpArtifact::Stream(s)    => run_stream_pipeline(s, spool_path, recipients, cfg).await,
        DumpArtifact::Directory(d) => run_directory_pipeline(d, spool_path, recipients, cfg).await,
    }
    // _permit drop → 다음 덤프 진입 허용. 스풀 커밋 시점에 DB 스냅샷 이미 해제(R7).
}
```

### 8. 팬아웃 — JoinSet 독립 업로드 + 부분 성공 (R6/R7)

커밋된 스풀(+서명 매니페스트는 S08)을 목적지별로 **독립** 업로드한다. 한 목적지 실패가 다른 목적지를
막지 않는다. 결과는 성공/실패가 섞인 벡터로 반환(부분 성공).

```rust
pub struct DestOutcome {
    pub dest_name: String,
    pub result: Result<UploadReceipt, UploadError>,   // 목적지별 개별 성패
    pub attempts: u32,
}

async fn fan_out(
    commit: &SpoolCommit,
    dests: Vec<std::sync::Arc<dyn Destination>>,
    cfg: &PipelineCfg,
) -> Vec<DestOutcome> {
    let mut set: tokio::task::JoinSet<DestOutcome> = tokio::task::JoinSet::new();

    for dest in dests {
        let spool = commit.spool_path.clone();
        let retries = cfg.upload_retries;
        set.spawn(async move {
            let name = dest.name().to_string();
            let mut attempts = 0;
            let mut last: Result<UploadReceipt, UploadError> = Err(UploadError::NotAttempted);
            // 목적지별 retry/resume. resume 가능 여부는 DestCaps.resumable가 결정,
            // 실제 이어올리기(multipart/offset)는 Destination이 담당(S06/S07).
            while attempts <= retries {
                attempts += 1;
                match dest.upload_resumable(&spool).await {
                    Ok(r)  => { last = Ok(r); break; }
                    Err(e) if e.is_retryable() && attempts <= retries => {
                        let backoff = std::time::Duration::from_millis(200 * (1 << (attempts - 1)));
                        tracing::warn!(%name, attempts, ?e, "업로드 재시도");
                        tokio::time::sleep(backoff).await;
                        last = Err(e);
                    }
                    Err(e) => { last = Err(e); break; }
                }
            }
            DestOutcome { dest_name: name, result: last, attempts }
        });
    }

    let mut outcomes = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(o)  => outcomes.push(o),
            Err(e) => outcomes.push(DestOutcome {   // 태스크 패닉도 실패로 수렴(전체 중단 X)
                dest_name: "<panicked>".into(),
                result: Err(UploadError::TaskPanic(e.to_string())),
                attempts: 0,
            }),
        }
    }
    outcomes   // 부분 성공 벡터 — 호출부가 "적어도 1개 불변 목적지 성공" 정책으로 판정
}
```
> 팬아웃 결과 정책(호출부): 최소 1개 목적지 성공이면 백업은 "부분 성공"으로 카탈로그에 기록하되,
> **요구 불변성 레벨(Object Lock 등)을 만족하는 목적지가 0개면 전체 실패**로 취급(P1-2, S06과 계약).

## Test Scenarios

| # | 시나리오 | 기대 | 검증 포인트 |
|---|---------|------|------------|
| T1 | 정상 Stream(PG `-Fc`) 파이프 | 스풀 생성·복호 성공·평문 blake3 일치 | R1/R3 happy path |
| T2 | **덤프 중간 truncation**: 자식이 stdout 도중 kill(SIGKILL 상당) | 스풀 파일 **삭제됨**, `bail`, 카탈로그 미커밋 | R2/R4 — 절단이 유효 백업으로 안 남음 |
| T3 | **EOF 도달 후 exit≠0**: stdout은 정상 종료했으나 자식 exit=1(권한/DDL 등) | 스풀 삭제, FAIL | R2 — EOF≠성공 |
| T4 | 치명 stderr 패턴(예: `server version mismatch`) + exit=0 | FAIL | `is_fatal_stderr` |
| T5 | finish 스킵 주입(테스트 훅으로 `commit()` 미호출 후 drop) | debug 빌드 **panic** | R5 FinishGuard |
| T6 | age `finish()` 실패 주입(디스크 가득) | 스풀 삭제, Err — 절단 파일 잔존 없음 | R3/R4 |
| T7 | 라운드트립: 스풀 → age decrypt → zstd decode → 평문 재해시 = 커밋 해시 | 일치 | P1-3(verify L1과 동일 경로) |
| T8 | Directory(mydumper) → tar → 체인 → 복호 후 tar 풀기 | 원본 디렉토리 트리 복원, TempDir **삭제 확인** | R10/R11, M2 |
| T9 | 팬아웃 **부분 성공**: 3목적지 중 1개(SSH) 네트워크 실패 | 벡터에 2 Ok + 1 Err, 다른 업로드 완료됨 | R6 — 격리 |
| T10 | 팬아웃 재시도: 목적지가 첫 2회 retryable 후 성공 | `attempts=3`, `Ok` | retry/backoff |
| T11 | 세마포어: `max_parallel_dumps=2`, 5잡 투입 | 동시 실행 ≤2 | R8 |
| T12 | 사전 디스크 가드: 여유 < `min_free_disk` | 덤프 시작 전 거부(스풀 파일 미생성) | R9 |
| T13 | 대용량(예: 5GB) 스트림 | 상수 메모리(스트리밍), 스풀 완결 | 메모리 회귀 가드 |
| T14 | 팬아웃 태스크 panic | 벡터에 `TaskPanic`, 전체 미중단 | JoinSet join 처리 |

테스트 하네스 힌트: T2/T3/T6은 **가짜 자식**(설정 가능한 exit/절단 지점을 갖는 테스트 바이너리 또는
`Cursor` 기반 fake reader + 주입형 `wait` 퓨처)으로 결정적으로 재현한다. `age::Encryptor`는
recipient 모드로 테스트용 X25519 identity를 써서 라운드트립을 닫는다.

## Implementation Strategy

1. **체인 골격 먼저(T1/T7)**: `HashingWriter`→`zstd`→`age`→`File`를 조립하고 라운드트립을 닫는다.
   여기서 finish 순서·평문 tee 위치를 못박는다(하중 리스크 조기 증명 — plan 슬라이스1).
2. **Commit Protocol 규율(T2/T3/T5/T6)**: `FinishGuard`, `?`-없는 크리티컬 구간, 단일 정리 경로,
   `wait` 기반 exit 확인. 실패 주입 테스트로 "절단 스풀 잔존 0"을 증명.
3. **Directory 경로(T8)**: tar-stream + TempDir 안전 삭제.
4. **동시성 가드(T11/T12)**: 세마포어 + 디스크 프리플라이트.
5. **팬아웃(T9/T10/T14)**: JoinSet + 목적지별 retry, 부분 성공 벡터. 실제 업로드는 S06 mock로 대역.

## Quality Gate

- `cargo test` — 위 T1~T14 전부 통과(특히 T2/T3/T5/T6 실패 경로).
- `cargo clippy -- -D warnings`, `cargo fmt --check`.
- **회귀 불변식**: 어떤 실패 경로에서도 (a) 미완결 스풀이 디스크에 남지 않는다, (b) `SpoolCommit`은
  finish+fsync+exit0을 모두 통과했을 때만 생성된다.
- 메모리 가드: T13에서 RSS가 입력 크기에 선형 증가하지 않음(스트리밍 확인).
- `cargo deny check` — `age`/`zstd` 핀 및 라이선스(H6).

## Risk & Rollback

| 리스크 | 영향 | 완화 |
|--------|------|------|
| **age 0.12 recipient API 변동**(BETA) | 컴파일/암호화 실패 | `=0.12.1` 핀, `wrap_output` 시그니처 빌드 시 확인 `[확인 필요]`. 롤백: 대안 chacha20poly1305 STREAM 자체 프레이밍(연구 §1) |
| **Directory 평문 TempDir spill**(M2 위반 소지) | 평문이 잠깐 디스크 잔존 | ① 가능하면 Stream 모드 선호, ② TempDir를 보호 볼륨/OS 임시암호화 위치에, ③ tar 직후 `secure_wipe_tempdir` 즉시 삭제, ④ 잔여 시간 최소화. 완전 무-spill은 Stream만 보장 — 문서에 명시 |
| **SyncIoBridge 런타임 의존** | blocking 태스크 데드락 | 반드시 `spawn_blocking` 내부에서만 생성/구동, 멀티스레드 런타임 사용 `[확인 필요]` |
| **fsync 미보장 파일시스템** | 전원 유실 시 스풀 손상 | `sync_all` 후에만 커밋. 실패 시 스풀 삭제 |
| 팬아웃 무한 재시도 | 자원 고갈 | `upload_retries` 상한 + 지수 backoff, non-retryable 즉시 종료 |

**Rollback**: 이 섹션은 순수 라이브러리 계층(부작용은 스풀 파일 생성/삭제 + 자식 프로세스). 커밋 실패는
스풀 삭제로 원자적 복구. 상위(잡 실행/스케줄)에 side effect를 누출하지 않으므로 기능 플래그로 옵션 C
파이프라인 자체를 비활성화하면 이전 슬라이스 상태로 되돌아간다.

## Acceptance Criteria

- [ ] Stream/Directory 양 아티팩트가 동일 인코딩 체인(blake3 tee → zstd → age → 스풀)을 통과한다(R1/R11).
- [ ] 평문 blake3 해시가 압축 이전 평문에 대해 스트리밍 계산되고 `SpoolCommit`에 담긴다(P1-3).
- [ ] finish가 **명시적 inner→outer**(zstd→age→flush/fsync)로 실행되고, drop에 의존하지 않는다(R3).
- [ ] debug 빌드에서 finish 없이 drop되면 panic한다(R5).
- [ ] 스트림 복사~커밋 구간에 `?` 조기반환이 없고, 모든 실패가 단일 정리 경로(스풀 삭제)로 수렴한다(R4).
- [ ] EOF에 도달했어도 자식 exit≠0/치명 stderr이면 스풀을 폐기하고 실패한다(R2).
- [ ] 커밋된 스풀만 팬아웃으로 진입하고, 스풀 write 완료 즉시 DB 스냅샷(세마포어 permit)이 해제된다(R7).
- [ ] 팬아웃은 목적지별 독립 업로드(JoinSet)로 retry/resume하며 부분 성공 벡터를 반환한다(R6).
- [ ] `max_parallel_dumps` 세마포어와 `min_free_disk` 프리플라이트가 동작한다(R8/R9).
- [ ] 암호화 스풀 외 평문 임시 파일이 남지 않는다(Stream은 무-spill, Directory는 tar 직후 삭제)(R10/M2).
- [ ] T1~T14 테스트 전부 통과.

## Files

| 파일 | 내용 |
|------|------|
| `src/pipeline/mod.rs` | 파이프라인 진입점, `PipelineCfg`, `run_job` |
| `src/pipeline/artifact.rs` | `DumpArtifact`/`DumpStream`/`DumpDir`/`DumpExit` 타입 |
| `src/pipeline/chain.rs` | `HashingWriter`, `EncryptChain`, `FinishGuard`, 타입 별칭 |
| `src/pipeline/commit.rs` | Commit Protocol: `drive_stream_blocking`, `run_stream_pipeline`, `run_directory_pipeline`, `is_fatal_stderr`, `secure_wipe_tempdir` |
| `src/pipeline/fanout.rs` | `fan_out`, `DestOutcome`, retry/backoff |
| `src/pipeline/guard.rs` | `preflight_disk_guard`(fs2), 세마포어 유틸 |
| `tests/pipeline_commit.rs` | T1~T8(체인/커밋/절단/finish) |
| `tests/pipeline_fanout.rs` | T9~T14(팬아웃/세마포어/디스크) |
| `tests/support/fake_dump.rs` | 설정형 fake 자식/reader/wait 퓨처 |

---
`#tags: pipeline, commit-protocol, option-c, spool-fanout, zstd, age, blake3, joinset, rust, section-s05`
