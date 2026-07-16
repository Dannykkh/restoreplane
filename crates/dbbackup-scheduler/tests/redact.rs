// T9~T12 — 시크릿 리댁션.
use dbbackup_scheduler::redact::{scrub, RedactingWriter};
use std::io::Write;

#[test]
fn t9_enc_field_is_redacted() {
    let out = scrub("password=enc:3:aGVsbG8=");
    assert!(!out.contains("aGVsbG8"), "base64 원문이 남음: {out}");
    assert!(!out.contains("enc:3"), "enc 원문이 남음: {out}");
}

#[test]
fn t10_dsn_password_is_redacted() {
    let out = scrub("postgres://backup_reader:hunter2@10.0.0.5/app");
    assert!(!out.contains("hunter2"), "DSN 비밀번호가 남음: {out}");
    assert!(out.contains("[REDACTED]"));
}

#[test]
fn t11_keyword_values_are_redacted() {
    assert!(!scrub("dek=abcdef0123456789").contains("abcdef0123456789"));
    assert!(!scrub("Recovery-Code: XXXX-YYYY").contains("XXXX"));
    // 가짜 API 키.
    assert!(!scrub("api_key=sk-live-1234567890abcdef").contains("sk-live-1234567890abcdef"));
    // KEK.
    assert!(!scrub("kek = TOPSECRETVALUE").contains("TOPSECRETVALUE"));
}

#[test]
fn t12_redacting_writer_is_last_line_of_defense() {
    // 호출부가 scrub()을 빠뜨린 상황을 시뮬레이션: 시크릿을 그대로 write해도 파일 바이트에는 남지 않는다.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scheduler.log");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut w = RedactingWriter::new(file);
        w.write_all(b"leak token=deadbeefsecret and postgres://u:p@h/db\n")
            .unwrap();
        w.flush().unwrap();
    }
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(!contents.contains("deadbeefsecret"), "writer가 토큰을 흘림: {contents}");
    assert!(!contents.contains("://u:p@"), "writer가 DSN을 흘림: {contents}");
    assert!(contents.contains("[REDACTED]"));
}
