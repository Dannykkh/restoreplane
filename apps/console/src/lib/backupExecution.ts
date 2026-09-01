import type { ServiceHealth } from "./types";

export type EvidenceMode = "service" | "browser_fixture";

export interface BackupExecutionGate {
  allowed: boolean;
  reason: string;
}

export function backupExecutionGate(
  health: ServiceHealth,
  evidenceMode: EvidenceMode,
): BackupExecutionGate {
  if (evidenceMode === "browser_fixture") {
    return {
      allowed: true,
      reason: "브라우저 검증 데이터로 실행 흐름을 시연합니다.",
    };
  }
  if (!health.acceptsIpc) {
    return {
      allowed: false,
      reason: "서비스가 현재 IPC 요청을 받지 않습니다.",
    };
  }
  if (health.executionAuthority !== "service") {
    return {
      allowed: false,
      reason: "서비스 실행 권한 전환이 완료되지 않았습니다.",
    };
  }
  if (
    health.phase !== "production_ready" ||
    health.cutoverMode !== "production" ||
    !health.claimsEnabled
  ) {
    return {
      allowed: false,
      reason: "운영 실행 claim이 활성화되지 않았습니다.",
    };
  }
  return {
    allowed: true,
    reason: "서비스가 durable backup 실행 권한을 보유합니다.",
  };
}
