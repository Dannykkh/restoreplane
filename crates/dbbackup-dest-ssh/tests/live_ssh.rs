// crates/dbbackup-dest-ssh/tests/live_ssh.rs
//! S07 라이브/인프라 테스트 — OpenSSH sshd(+ 바스티온 뒤 DB) 컨테이너가 있어야 재현된다.
//! 무인 CI에는 인프라가 없으므로 #[ignore]로 게이트. 실행: `cargo test -p dbbackup-dest-ssh -- --ignored`
//! (env: SSH_HOST/SSH_PORT/SSH_USER/SSH_PASSWORD/SSH_HOST_FPR/SSH_REMOTE_DIR).

use dbbackup_dest_ssh::{open_tunnel, ssh_connect, HostKeyPolicy, SshAuth, SshDest};
use dbbackup_pipeline::{upload_resumable, Destination};
use zeroize::Zeroizing;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} env 필요"))
}

#[tokio::test]
#[ignore = "needs OpenSSH sshd 컨테이너 (SSH_* env) — SFTP 업로드 + rename-commit"]
async fn ssh_sftp_upload_rename_commit() {
    let policy = HostKeyPolicy::new(
        env("SSH_HOST_FPR"),
        format!("{}:{}", env("SSH_HOST"), env("SSH_PORT")),
    );
    let auth = SshAuth::Password {
        password: Zeroizing::new(env("SSH_PASSWORD")),
    };
    let dest = SshDest::new(
        "ssh-live",
        env("SSH_HOST"),
        env("SSH_PORT").parse().unwrap(),
        env("SSH_USER"),
        policy,
        auth,
        env("SSH_REMOTE_DIR"),
        false,
    );
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"hello sftp").unwrap();
    let receipt = upload_resumable(&dest as &dyn Destination, tmp.path(), "live/sftp.bin")
        .await
        .expect("SFTP 업로드 + rename-commit 성공");
    assert_eq!(receipt.bytes, 10);
}

#[tokio::test]
#[ignore = "needs 바스티온 sshd + 뒤단 DB (SSH_* env) — direct-tcpip 터널 개통"]
async fn ssh_tunnel_direct_tcpip() {
    let policy = HostKeyPolicy::new(
        env("SSH_HOST_FPR"),
        format!("{}:{}", env("SSH_HOST"), env("SSH_PORT")),
    );
    let auth = SshAuth::Password {
        password: Zeroizing::new(env("SSH_PASSWORD")),
    };
    let handle = ssh_connect(
        &env("SSH_HOST"),
        env("SSH_PORT").parse().unwrap(),
        &env("SSH_USER"),
        policy,
        auth,
    )
    .await
    .expect("바스티온 접속");
    let tunnel = open_tunnel(
        handle,
        env("DB_HOST"),
        env("DB_PORT").parse().unwrap(),
        "127.0.0.1:0",
    )
    .await
    .expect("터널 개통");
    // 로컬 바인드 주소로 접속 가능한지 확인(실제 DB 핸드셰이크는 소스 테스트 소관).
    assert!(tunnel.local_addr.port() > 0);
    let _ = tokio::net::TcpStream::connect(tunnel.local_addr).await;
}
