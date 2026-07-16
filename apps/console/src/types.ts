// 백엔드(dbbackup-config / catalog)가 돌려주는 구조의 프론트 뷰 타입.
// src-tauri commands.rs 의 serde 직렬화 결과와 형태를 맞춘다.

export type VerifyLevel = 1 | 2 | 3;

export type ImmutabilityKind =
  | "none"
  | "server_append_only"
  | "object_lock_governance"
  | "object_lock_compliance";

export interface DestArtifact {
  destName: string;
  destKind: string; // local | s3 | ssh
  immutability: ImmutabilityKind;
  retentionUntil: string | null;
  status: string; // uploaded | pending | failed
}

export interface BackupRow {
  id: string;
  job: string;
  source: string;
  engine: string;
  startedAt: string;
  finishedAt: string | null;
  status: string; // success | running | failed
  plaintextBytes: number | null;
  cipherBytes: number | null;
  manifestSigned: boolean;
  lastVerifyLevel: VerifyLevel | null;
  lastVerifyResult: string | null; // pass | fail | null
  artifacts: DestArtifact[];
}

export interface JobSummary {
  name: string;
  source: string;
  engine: string;
  schedule: string;
  requiredImmutability: string;
  destinations: string[];
  lastBackup: BackupRow | null;
  health: "ok" | "warn" | "danger" | "unknown";
}

export interface DashboardData {
  configPresent: boolean;
  keystorePresent: boolean;
  schemaVersion: number | null;
  jobs: JobSummary[];
  recent: BackupRow[];
  overall: "ok" | "warn" | "danger" | "empty";
}
