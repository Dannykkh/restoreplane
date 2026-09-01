import type { BackupScopeMode } from "../lib/types";
import { StatusBadge } from "./StatusBadge";

interface ScopeCoverageProps {
  mode: BackupScopeMode;
  selectedObjectCount: number | null;
  eligibleObjectCount: number | null;
  newObjectCount?: number | null;
  missingObjectCount?: number | null;
  changedObjectCount?: number | null;
}

export function ScopeCoverage({
  mode,
  selectedObjectCount,
  eligibleObjectCount,
  newObjectCount = null,
  missingObjectCount = null,
  changedObjectCount = null,
}: ScopeCoverageProps) {
  if (mode === "whole_database") {
    return (
      <span className="role-pair" aria-label="Backup scope coverage">
        <StatusBadge label="전체 데이터베이스" tone="info" />
        <span className="muted-copy">현재 및 이후 생성되는 테이블 포함</span>
      </span>
    );
  }

  return (
    <span className="role-pair" aria-label="Backup scope coverage">
      <span className="mono">
        <strong>{selectedObjectCount ?? "?"} / {eligibleObjectCount ?? "?"}</strong>
      </span>
      <span className="muted-copy">선택 테이블</span>
      {eligibleObjectCount === null && (
        <StatusBadge label="Discovery 근거 없음" tone="neutral" />
      )}
      {(newObjectCount ?? 0) > 0 && (
        <span role="status">
          <StatusBadge label={`새 테이블 ${newObjectCount ?? 0}개 미보호`} tone="warning" />
        </span>
      )}
      {(missingObjectCount ?? 0) > 0 && (
        <span role="alert">
          <StatusBadge label={`선택 테이블 ${missingObjectCount ?? 0}개 누락`} tone="danger" />
        </span>
      )}
      {(changedObjectCount ?? 0) > 0 && (
        <span role="status">
          <StatusBadge label={`정의 변경 ${changedObjectCount ?? 0}개`} tone="warning" />
        </span>
      )}
    </span>
  );
}
