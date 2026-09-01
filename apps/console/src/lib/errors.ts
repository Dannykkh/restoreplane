const ERROR_COPY: Record<string, { title: string; action: string }> = {
  IPC_PIPE_CONNECT_FAILED: {
    title: "백업 서비스에 연결할 수 없습니다.",
    action: "Windows 서비스 상태를 확인한 뒤 다시 연결하세요.",
  },
  IPC_PIPE_TIMEOUT: {
    title: "서비스 응답 시간이 초과되었습니다.",
    action: "실행 중인 작업은 취소되지 않았습니다. Activity에서 상태를 다시 확인하세요.",
  },
  APPLICATION_FORBIDDEN: {
    title: "이 작업을 실행할 권한이 없습니다.",
    action: "설치 소유자 또는 승인된 관리자로 다시 시도하세요.",
  },
  APPLICATION_REHEARSAL_ONLY: {
    title: "현재 빌드는 실운영 작업을 실행하지 않습니다.",
    action: "Settings에서 전환 증거와 실행 권한 상태를 확인하세요.",
  },
  APPLICATION_COMMAND_UNKNOWN: {
    title: "현재 서비스 버전이 이 작업을 지원하지 않습니다.",
    action: "서비스와 콘솔 버전을 맞춘 뒤 다시 시도하세요.",
  },
  APPLICATION_PAYLOAD_INVALID: {
    title: "입력값을 처리할 수 없습니다.",
    action: "표시된 필드를 확인한 뒤 다시 시도하세요.",
  },
  APPLICATION_OUTCOME_UNKNOWN: {
    title: "이전 요청의 최종 결과를 확정할 수 없습니다.",
    action: "같은 작업을 다시 실행하지 말고 현재 상태와 Activity를 새로 확인하세요.",
  },
  IPC_REQUEST_INCOMPLETE: {
    title: "이전 요청이 아직 완료 상태로 확정되지 않았습니다.",
    action: "같은 작업을 다시 실행하지 말고 현재 상태와 Activity를 새로 확인하세요.",
  },
  OPERATIONAL_REPOSITORY_NOT_FOUND: {
    title: "요청한 대상을 찾을 수 없습니다.",
    action: "목록을 새로 고친 뒤 대상을 다시 선택하세요.",
  },
  PLAN_REVISION_TARGET_NOT_READY: {
    title: "백업 대상 연결이 아직 준비되지 않았습니다.",
    action: "Connections에서 최신 연결 Probe와 데이터베이스 선택 상태를 확인하세요.",
  },
  PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE: {
    title: "선택한 엔진 기능을 현재 인증 패키지가 보장하지 않습니다.",
    action: "Settings에서 엔진 인증 상태를 확인하거나 지원되는 검증 수준을 선택하세요.",
  },
  PLAN_REVISION_DESTINATION_NOT_READY: {
    title: "백업 저장소의 쓰기·복원 읽기 준비가 확인되지 않았습니다.",
    action: "Destinations에서 Backup writer와 Restore reader Probe를 모두 다시 실행하세요.",
  },
  PLAN_REVISION_VERIFICATION_UNSUPPORTED: {
    title: "선택한 자동 검증 수준을 이 엔진이 지원하지 않습니다.",
    action: "엔진 인증 범위 안의 검증 수준으로 Plan을 수정하세요.",
  },
};

export class ServiceError extends Error {
  readonly code: string;
  readonly requestId: string | null;
  readonly retryable: boolean;
  readonly action: string;

  constructor(
    code: string,
    options?: { requestId?: string | null; retryable?: boolean; cause?: unknown },
  ) {
    const copy = ERROR_COPY[code] ?? {
      title: "서비스가 요청을 완료하지 못했습니다.",
      action: "Activity에서 요청 상태를 확인하고 다시 시도하세요.",
    };
    super(copy.title);
    this.name = "ServiceError";
    this.code = code;
    this.requestId = options?.requestId ?? null;
    this.retryable = options?.retryable ?? false;
    this.action = copy.action;
  }
}

export function asServiceError(error: unknown): ServiceError {
  if (error instanceof ServiceError) return error;
  const code =
    typeof error === "string" && /^[A-Z][A-Z0-9_]{2,127}$/.test(error)
      ? error
      : "IPC_PIPE_CONNECT_FAILED";
  return new ServiceError(code, { cause: error, retryable: true });
}
