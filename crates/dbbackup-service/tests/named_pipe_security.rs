#![cfg(windows)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use uuid::Uuid;

use dbbackup_service::ipc::{
    audit_pipe_dacl_sddl, current_process_identity, identity_from_connected_pipe,
    secure_named_pipe_server, PipeSecurityPolicy, WindowsSid,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kernel_dacl_and_impersonated_identity_match_the_local_client_token() {
    let current = current_process_identity().expect("query current process token");
    let system = WindowsSid::parse("S-1-5-18").expect("SYSTEM SID");
    let local_service = WindowsSid::parse("S-1-5-19").expect("LOCAL SERVICE SID");
    let service_sid = if current.user_sid() == &system {
        local_service
    } else {
        system
    };
    let security = PipeSecurityPolicy::new(service_sid, current.user_sid().clone(), Vec::new())
        .expect("pipe security policy");
    let pipe_name = format!(r"\\.\pipe\DbBackup.test.{}", Uuid::new_v4());
    let mut server =
        secure_named_pipe_server(&pipe_name, &security, true).expect("create secured pipe");

    let actual_sddl = audit_pipe_dacl_sddl(&server).expect("read kernel pipe DACL");
    assert!(actual_sddl.starts_with("D:P"));
    let owner_is_present = actual_sddl.contains(current.user_sid().as_str())
        || (current.user_sid().as_str().ends_with("-500") && actual_sddl.contains(";;;LA"));
    assert!(
        owner_is_present,
        "owner SID {} missing from {actual_sddl}",
        current.user_sid().as_str()
    );
    assert!(!actual_sddl.contains(";;;WD"));
    assert!(!actual_sddl.contains(";;;AU"));

    let duplicate = secure_named_pipe_server(&pipe_name, &security, true)
        .expect_err("first-instance protection rejects a competing listener");
    assert_eq!(duplicate.code(), "IPC_PIPE_CREATE_FAILED");

    let mut client = ClientOptions::new()
        .open(&pipe_name)
        .expect("open local client");
    client
        .write_all(b"identity")
        .await
        .expect("write authentication message");
    server.connect().await.expect("accept client");
    let mut message = [0_u8; 8];
    server
        .read_exact(&mut message)
        .await
        .expect("read authentication message");
    assert_eq!(&message, b"identity");

    let impersonated =
        identity_from_connected_pipe(&server).expect("impersonate and query client token");
    assert_eq!(impersonated.user_sid(), current.user_sid());
    assert_eq!(impersonated.logon_sid(), current.logon_sid());
    assert_eq!(impersonated.session_id(), current.session_id());
    assert_eq!(impersonated.elevated_admin(), current.elevated_admin());
    assert!(impersonated.local());
}
