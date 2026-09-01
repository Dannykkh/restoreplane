#![cfg(windows)]

use restoreplane_service::host::HostPhase;
use restoreplane_service::scm::{shutdown_pending_status, status_for_phase, SERVICE_NAME};
use windows_service::service::{ServiceControlAccept, ServiceState};

#[test]
fn scm_status_contract_keeps_operational_draining_controllable_and_separates_shutdown() {
    assert_eq!(SERVICE_NAME, "RestorePlaneService");

    let starting = status_for_phase(HostPhase::Starting);
    assert_eq!(starting.current_state, ServiceState::StartPending);
    assert!(starting.controls_accepted.is_empty());

    for phase in [
        HostPhase::RehearsalReady,
        HostPhase::ProductionReady,
        HostPhase::DiagnosticsOnly,
    ] {
        let running = status_for_phase(phase);
        assert_eq!(running.current_state, ServiceState::Running);
        assert_eq!(
            running.controls_accepted,
            ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN
        );
    }

    let draining = status_for_phase(HostPhase::Draining);
    assert_eq!(draining.current_state, ServiceState::Running);
    assert_eq!(
        draining.controls_accepted,
        ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN
    );

    let shutdown_pending = shutdown_pending_status();
    assert_eq!(shutdown_pending.current_state, ServiceState::StopPending);
    assert!(shutdown_pending.controls_accepted.is_empty());

    let stopped = status_for_phase(HostPhase::Stopped);
    assert_eq!(stopped.current_state, ServiceState::Stopped);
    assert!(stopped.controls_accepted.is_empty());
}
