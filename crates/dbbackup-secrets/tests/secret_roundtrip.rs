// crates/dbbackup-secrets/tests/secret_roundtrip.rs
//! S02 통합 테스트(T1~T15 부분집합). wrap 3경로 라운드트립, payload STREAM, 서명/변조,
//! rotate-key, 복구코드 생성/검증을 커버한다.

use dbbackup_secrets::{
    generate_recovery_code, generate_signing_keypair, sign_manifest, verify_manifest, Credential,
    DekEncryptor, RecoveryVerifier, SecretManager,
};
use dbbackup_core::SecretEncryptor;
use std::path::PathBuf;
use zeroize::Zeroizing;

fn temp_keyfile(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let uniq = format!(
        "dbbackup-dpapi-{}-{}-{}.key",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    p.push(uniq);
    p
}

fn manager(tag: &str, recovery_code: &str) -> SecretManager {
    let (sk, vk) = generate_signing_keypair();
    let verifier = RecoveryVerifier::from_code(recovery_code).unwrap();
    SecretManager::new(temp_keyfile(tag), Some(sk), vk, Some(verifier))
}

// T1: DEK 발급마다 서로 다른 값(정적 아님, C2). issue_and_wrap 두 번의 ct가 다름으로 간접 확인.
#[test]
fn t1_dek_is_fresh_each_backup() {
    let code = generate_recovery_code();
    let mgr = manager("t1", &code);
    let pw = Zeroizing::new("master-pw-1".to_string());
    let (_d1, set1) = mgr.issue_and_wrap(&pw, &code, 1).unwrap();
    let (_d2, set2) = mgr.issue_and_wrap(&pw, &code, 1).unwrap();
    // 같은 비번이라도 DEK/salt/nonce가 달라 wrap ct가 달라야 한다.
    let ct1 = serde_json::to_string(&set1.stanzas).unwrap();
    let ct2 = serde_json::to_string(&set2.stanzas).unwrap();
    assert_ne!(ct1, ct2);
}

// T2: 비밀번호 wrap → unwrap 동일 DEK, 잘못된 비번은 실패.
#[test]
fn t2_password_wrap_unwrap() {
    let code = generate_recovery_code();
    let mgr = manager("t2", &code);
    let pw = Zeroizing::new("correct horse battery".to_string());
    let (dek, set) = mgr.issue_and_wrap(&pw, &code, 1).unwrap();

    let recovered = mgr
        .unwrap(&set, &Credential::Password(pw.clone()))
        .unwrap();
    assert_eq!(expose(&dek), expose(&recovered));

    let bad = Zeroizing::new("wrong password".to_string());
    assert!(mgr.unwrap(&set, &Credential::Password(bad)).is_err());
}

// T3: 복구코드 wrap → unwrap, 1글자 변경 시 실패.
#[test]
fn t3_recovery_wrap_unwrap() {
    let code = generate_recovery_code();
    let mgr = manager("t3", &code);
    let pw = Zeroizing::new("pw".to_string());
    let (dek, set) = mgr.issue_and_wrap(&pw, &code, 1).unwrap();

    let recovered = mgr
        .unwrap(&set, &Credential::Recovery(code.clone()))
        .unwrap();
    assert_eq!(expose(&dek), expose(&recovered));

    // 마지막 글자 변경 → 검증자/AEAD 모두 거부.
    let mut tampered = code.to_string();
    let last = tampered.pop().unwrap();
    let repl = if last == 'A' { 'B' } else { 'A' };
    tampered.push(repl);
    let tampered = Zeroizing::new(tampered);
    assert!(mgr.unwrap(&set, &Credential::Recovery(tampered)).is_err());
}

// T4/T12: DPAPI wrap → unwrap(동일 머신+keyfile). 비Windows는 스킵.
#[cfg(windows)]
#[test]
fn t4_dpapi_wrap_unwrap() {
    let code = generate_recovery_code();
    let mgr = manager("t4", &code);
    let pw = Zeroizing::new("pw".to_string());
    let (dek, set) = mgr.issue_and_wrap(&pw, &code, 1).unwrap();

    let recovered = mgr.unwrap(&set, &Credential::Dpapi).unwrap();
    assert_eq!(expose(&dek), expose(&recovered));
}

// T6: payload STREAM 암/복호 라운드트립(다중 청크).
#[test]
fn t6_payload_stream_roundtrip() {
    let code = generate_recovery_code();
    let mgr = manager("t6", &code);
    let pw = Zeroizing::new("pw".to_string());
    let (dek, _set) = mgr.issue_and_wrap(&pw, &code, 1).unwrap();

    let nonce_prefix = [7u8; 7];
    let chunk_a = b"first-chunk-of-dump-".to_vec();
    let chunk_b = b"second-and-final".to_vec();

    let mut enc = dek.payload_encryptor(&nonce_prefix);
    let ca = enc.encrypt_next(chunk_a.as_slice()).unwrap();
    let cb = enc.encrypt_last(chunk_b.as_slice()).unwrap();

    let mut dec = dek.payload_decryptor(&nonce_prefix);
    let pa = dec.decrypt_next(ca.as_slice()).unwrap();
    let pb = dec.decrypt_last(cb.as_slice()).unwrap();

    assert_eq!(pa, chunk_a);
    assert_eq!(pb, chunk_b);

    // finish/last 순서를 어기면(중간 청크로 오인) 복호 실패.
    let dec2 = dek.payload_decryptor(&nonce_prefix);
    assert!(dec2.decrypt_last(ca.as_slice()).is_err());
}

// T8/T9/T10: 서명 → 검증, 1바이트 변조 거부.
#[test]
fn t8_sign_verify_and_tamper() {
    let (sk, vk) = generate_signing_keypair();
    let manifest = b"canonical-manifest-bytes-v1";
    let sig = sign_manifest(&sk, manifest);
    assert!(verify_manifest(&vk, manifest, &sig).is_ok());

    let mut tampered = manifest.to_vec();
    tampered[0] ^= 0x01;
    assert!(verify_manifest(&vk, &tampered, &sig).is_err());
}

// T11: rotate-key 후 DEK 불변, 신 세대는 새 비번으로 unwrap, 세대 증가.
#[test]
fn t11_rotate_key_rewraps_without_changing_dek() {
    let code = generate_recovery_code();
    let mgr = manager("t11", &code);
    let old_pw = Zeroizing::new("old-pw".to_string());
    let (dek, set) = mgr.issue_and_wrap(&old_pw, &code, 1).unwrap();

    let new_pw = Zeroizing::new("new-pw".to_string());
    let new_code = generate_recovery_code();
    let new_set = mgr
        .rotate(
            &set,
            &Credential::Password(old_pw.clone()),
            &new_pw,
            &new_code,
            2,
        )
        .unwrap();

    assert_eq!(new_set.dek_wrap_gen, 2);
    // 새 세대는 새 비번으로 복구되며 DEK는 동일해야 한다(덤프 재암호화 0).
    let re = mgr
        .unwrap(&new_set, &Credential::Password(new_pw.clone()))
        .unwrap();
    assert_eq!(expose(&dek), expose(&re));
    // 구 비번은 새 세대에서 실패.
    assert!(mgr
        .unwrap(&new_set, &Credential::Password(old_pw))
        .is_err());
}

// T13: 3경로 중 손상 후 남은 경로로 복구(독립성). 여기서는 비번 스탠자만 남겨 복구.
#[test]
fn t13_independent_paths() {
    let code = generate_recovery_code();
    let mgr = manager("t13", &code);
    let pw = Zeroizing::new("pw".to_string());
    let (dek, mut set) = mgr.issue_and_wrap(&pw, &code, 1).unwrap();

    // 복구코드 스탠자를 손상시켜도 비번 경로로 복구 가능해야 한다.
    set.stanzas.retain(|s| {
        matches!(s, dbbackup_secrets::WrapStanza::Argon2id { .. })
    });
    assert!(!set.stanzas.is_empty());
    let re = mgr.unwrap(&set, &Credential::Password(pw)).unwrap();
    assert_eq!(expose(&dek), expose(&re));
}

// 복구코드 생성 → 검증(정확 코드만 통과).
#[test]
fn recovery_code_generate_and_verify() {
    let code = generate_recovery_code();
    let verifier = RecoveryVerifier::from_code(&code).unwrap();
    assert!(verifier.verify(&code));
    assert!(!verifier.verify("WRONG-CODE-XXXXX"));
    // 그룹 하이픈 포함 & 최소 엔트로피(20B base32 = 32글자 + 하이픈).
    assert!(code.contains('-'));
    assert!(code.replace('-', "").len() >= 26);
}

// SecretEncryptor(core 경계) 라운드트립 — config 필드 암/복호.
#[test]
fn dek_encryptor_field_roundtrip() {
    let code = generate_recovery_code();
    let mgr = manager("field", &code);
    let pw = Zeroizing::new("pw".to_string());
    let (dek, _set) = mgr.issue_and_wrap(&pw, &code, 3).unwrap();

    let enc = DekEncryptor::new(dek, 3);
    let field = enc.wrap("db-password-secret").unwrap();
    assert_eq!(field.wrap_gen, 3);
    let back = enc.unwrap_field(&field).unwrap();
    assert_eq!(&*back, "db-password-secret");
}

/// 테스트 전용: DEK 바이트 비교(라운드트립 검증). Dek는 Debug/expose 공개가 없으므로
/// 매니저를 통해 얻은 두 DEK가 같은 payload 키를 만드는지로 간접 비교한다.
fn expose(dek: &dbbackup_secrets::Dek) -> Vec<u8> {
    // 동일 nonce_prefix로 같은 평문을 암호화하면 같은 DEK는 같은 ct를 낸다.
    let np = [0u8; 7];
    let e = dek.payload_encryptor(&np);
    e.encrypt_last(b"probe".as_slice()).unwrap()
}
