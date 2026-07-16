// Tauri invoke 얇은 래퍼 + 브라우저 목 폴백.
// Tauri 런타임이 아니면(=플레인 Vite/브라우저) 목 데이터를 돌려줘 디자인을 단독 렌더/검증한다.
import type { DashboardData } from "../types";

function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

async function tauriInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args);
}

export async function getDashboard(): Promise<DashboardData> {
  if (inTauri()) return tauriInvoke<DashboardData>("dashboard");
  return MOCK_DASHBOARD;
}

// --- 초기 설정(init) ---
export interface KeystoreStatus {
  configPath: string;
  configPresent: boolean;
  keystorePresent: boolean;
}
export interface InitResult {
  recoveryCode: string;
  verifyingKeyHex: string;
  keystorePath: string;
}

let mockKeystoreCreated = false;

export async function keystoreStatus(): Promise<KeystoreStatus> {
  if (inTauri()) return tauriInvoke<KeystoreStatus>("keystore_status");
  return {
    configPath: "C:\\ProgramData\\dbbackup\\dbbackup.toml",
    configPresent: mockKeystoreCreated,
    keystorePresent: mockKeystoreCreated,
  };
}

export async function initKeystore(masterPassword: string): Promise<InitResult> {
  if (inTauri()) return tauriInvoke<InitResult>("init_keystore", { masterPassword });
  // 브라우저 목: 복구코드 표시 모달 디자인 검증용.
  if (masterPassword.trim().length < 8) throw new Error("마스터 비밀번호는 8자 이상이어야 합니다.");
  mockKeystoreCreated = true;
  return {
    recoveryCode: "K7QM-2XR9-4TJD-9WFA-M3PN-6HBE",
    verifyingKeyHex: "a1b2c3d4e5f60718293a4b5c6d7e8f90112233445566778899aabbccddeeff00",
    keystorePath: "C:\\ProgramData\\dbbackup\\keystore.json",
  };
}

// --- DB 등록 ---
export interface DbSummary {
  alias: string;
  engine: string;
  host: string;
  port: number;
  dbname: string;
  user: string;
  hasTunnel: boolean;
}
export interface NewDatabase {
  alias: string;
  engine: string;
  host: string;
  port: number;
  dbname: string;
  user: string;
  password: string;
  dumpGlobals: boolean;
}

const MOCK_DBS: DbSummary[] = [
  { alias: "prod-main", engine: "postgresql", host: "10.0.0.5", port: 5432, dbname: "app", user: "backup", hasTunnel: true },
  { alias: "billing-mysql", engine: "mysql", host: "10.0.0.9", port: 3306, dbname: "billing", user: "svc_backup", hasTunnel: false },
];

export async function listDatabases(): Promise<DbSummary[]> {
  if (inTauri()) return tauriInvoke<DbSummary[]>("list_databases");
  return MOCK_DBS;
}

export async function addDatabase(masterPassword: string, db: NewDatabase): Promise<void> {
  if (inTauri()) return tauriInvoke<void>("add_database", { masterPassword, db });
  if (masterPassword.trim().length < 8) throw new Error("마스터 비밀번호가 올바르지 않습니다.");
  if (MOCK_DBS.some((d) => d.alias === db.alias)) throw new Error(`이미 존재하는 alias입니다: ${db.alias}`);
  MOCK_DBS.push({
    alias: db.alias,
    engine: db.engine,
    host: db.host,
    port: db.port,
    dbname: db.dbname,
    user: db.user,
    hasTunnel: false,
  });
}

// --- 목적지 등록 ---
export interface DestSummary {
  name: string;
  kind: string; // local | s3 | ssh
  immutability: string; // none | server_append_only | object_lock_governance | object_lock_compliance
  location: string;
}
export interface NewDestination {
  name: string;
  kind: string;
  path?: string;
  endpoint?: string;
  region?: string;
  bucket?: string;
  prefix?: string;
  accessKeyId?: string;
  secretAccessKey?: string;
  objectLockMode?: string;
  retentionDays?: number;
  host?: string;
  port?: number;
  user?: string;
  privateKeyPath?: string;
  hostKeyFingerprint?: string;
}

const MOCK_DESTS: DestSummary[] = [
  { name: "s3-worm", kind: "s3", immutability: "object_lock_governance", location: "my-backups/prod/" },
  { name: "bastion-sftp", kind: "ssh", immutability: "server_append_only", location: "svc@bastion:/srv/backups" },
];

export async function listDestinations(): Promise<DestSummary[]> {
  if (inTauri()) return tauriInvoke<DestSummary[]>("list_destinations");
  return MOCK_DESTS;
}

export async function addDestination(masterPassword: string, dest: NewDestination): Promise<void> {
  if (inTauri()) return tauriInvoke<void>("add_destination", { masterPassword, dest });
  if (MOCK_DESTS.some((d) => d.name === dest.name)) throw new Error(`이미 존재하는 목적지 이름입니다: ${dest.name}`);
  if (dest.kind === "ssh" && !dest.hostKeyFingerprint) throw new Error("ssh 목적지는 host_key_fingerprint가 필요합니다(MITM 방어).");
  const immut =
    dest.kind === "ssh"
      ? "server_append_only"
      : dest.kind === "s3" && dest.objectLockMode === "governance"
        ? "object_lock_governance"
        : dest.kind === "s3" && dest.objectLockMode === "compliance"
          ? "object_lock_compliance"
          : "none";
  const location =
    dest.kind === "local"
      ? dest.path ?? ""
      : dest.kind === "s3"
        ? `${dest.bucket ?? ""}/${dest.prefix ?? ""}`
        : `${dest.user ?? ""}@${dest.host ?? ""}:${dest.path ?? ""}`;
  MOCK_DESTS.push({ name: dest.name, kind: dest.kind, immutability: immut, location });
}

// --- 백업 잡 등록 ---
export interface JobSummaryRow {
  name: string;
  source: string;
  destinations: string[];
  requiredImmutability: string;
  schedule: string;
  verifyLevel: number;
  enabled: boolean;
}
export interface NewJob {
  name: string;
  source: string;
  destinations: string[];
  requiredImmutability: string;
  retentionDaily: number;
  retentionWeekly: number;
  retentionMonthly: number;
  verifyLevel: number;
  schedule: string;
}

const MOCK_JOBS: JobSummaryRow[] = [
  {
    name: "prod-nightly",
    source: "prod-main",
    destinations: ["s3-worm", "bastion-sftp"],
    requiredImmutability: "governance",
    schedule: "0 3 * * *",
    verifyLevel: 3,
    enabled: true,
  },
];

export async function listJobs(): Promise<JobSummaryRow[]> {
  if (inTauri()) return tauriInvoke<JobSummaryRow[]>("list_jobs");
  return MOCK_JOBS;
}

export async function addJob(job: NewJob): Promise<void> {
  if (inTauri()) return tauriInvoke<void>("add_job", { job });
  if (!job.destinations.length) throw new Error("목적지를 1개 이상 선택하세요.");
  if (MOCK_JOBS.some((j) => j.name === job.name)) throw new Error(`이미 존재하는 잡 이름입니다: ${job.name}`);
  MOCK_JOBS.push({
    name: job.name,
    source: job.source,
    destinations: job.destinations,
    requiredImmutability: job.requiredImmutability,
    schedule: job.schedule,
    verifyLevel: job.verifyLevel,
    enabled: true,
  });
}

// --- 백업 실행(사이드카 스트리밍) ---
export interface RunLogLine {
  line: string;
  stream: "stdout" | "stderr";
}
export interface RunDone {
  code: number | null;
  ok: boolean;
}

/** 잡 실행. 진행 로그를 onLog로, 종료를 onDone으로 콜백. 정리 함수를 반환한다. */
export async function runJob(
  source: string,
  dryRun: boolean,
  onLog: (l: RunLogLine) => void,
  onDone: (d: RunDone) => void,
): Promise<() => void> {
  if (inTauri()) {
    const { listen } = await import("@tauri-apps/api/event");
    const un1 = await listen<RunLogLine>("run:log", (e) => onLog(e.payload));
    const un2 = await listen<RunDone>("run:done", (e) => onDone(e.payload));
    try {
      await tauriInvoke<void>("run_job", { source, dryRun });
    } catch (e) {
      onLog({ line: String(e), stream: "stderr" });
      onDone({ code: null, ok: false });
    }
    return () => {
      un1();
      un2();
    };
  }
  // 브라우저 목: 사이드카 dry-run 출력을 흉내내 로그 패널을 검증.
  const lines = dryRun
    ? [
        `[dry-run] would run job for source '${source}'`,
        `[dry-run] plan: dump -> blake3 tee -> zstd -> chacha20 STREAM -> spool`,
        `[dry-run] fan-out to registered destinations`,
      ]
    : [
        `[run] source '${source}' 실행은 라이브 DB 연결이 필요합니다(브라우저 목).`,
      ];
  let i = 0;
  const timer = setInterval(() => {
    if (i < lines.length) {
      onLog({ line: lines[i++], stream: "stdout" });
    } else {
      clearInterval(timer);
      onDone({ code: 0, ok: dryRun });
    }
  }, 350);
  return () => clearInterval(timer);
}

// --- 브라우저 디자인 검증용 목 데이터 (Tauri 밖에서만 사용) ---
const MOCK_DASHBOARD: DashboardData = {
  configPresent: true,
  keystorePresent: true,
  schemaVersion: 1,
  overall: "ok",
  jobs: [
    {
      name: "prod-nightly",
      source: "prod-main",
      engine: "postgresql",
      schedule: "매일 03:00",
      requiredImmutability: "object_lock",
      destinations: ["s3-worm", "bastion-sftp"],
      health: "ok",
      lastBackup: {
        id: "bk_2026071702593f",
        job: "prod-nightly",
        source: "prod-main",
        engine: "postgresql",
        startedAt: "2026-07-17 03:00:04",
        finishedAt: "2026-07-17 03:07:41",
        status: "success",
        plaintextBytes: 8_140_291_070,
        cipherBytes: 2_013_774_338,
        manifestSigned: true,
        lastVerifyLevel: 3,
        lastVerifyResult: "pass",
        artifacts: [
          {
            destName: "s3-worm",
            destKind: "s3",
            immutability: "object_lock_governance",
            retentionUntil: "2026-08-16",
            status: "uploaded",
          },
          {
            destName: "bastion-sftp",
            destKind: "ssh",
            immutability: "server_append_only",
            retentionUntil: null,
            status: "uploaded",
          },
        ],
      },
    },
    {
      name: "billing-weekly",
      source: "billing-mysql",
      engine: "mysql",
      schedule: "매주 일 04:15",
      requiredImmutability: "append_only_or_lock",
      destinations: ["s3-worm"],
      health: "warn",
      lastBackup: {
        id: "bk_20260712041500a1",
        job: "billing-weekly",
        source: "billing-mysql",
        engine: "mysql",
        startedAt: "2026-07-12 04:15:00",
        finishedAt: "2026-07-12 04:19:22",
        status: "success",
        plaintextBytes: 1_284_112_004,
        cipherBytes: 402_118_774,
        manifestSigned: true,
        lastVerifyLevel: 1,
        lastVerifyResult: "pass",
        artifacts: [
          {
            destName: "s3-worm",
            destKind: "s3",
            immutability: "object_lock_governance",
            retentionUntil: "2026-08-11",
            status: "uploaded",
          },
        ],
      },
    },
    {
      name: "analytics-daily",
      source: "analytics-pg",
      engine: "postgresql",
      schedule: "매일 02:30",
      requiredImmutability: "none",
      destinations: ["local-nas"],
      health: "danger",
      lastBackup: {
        id: "bk_20260717023004ff",
        job: "analytics-daily",
        source: "analytics-pg",
        engine: "postgresql",
        startedAt: "2026-07-17 02:30:04",
        finishedAt: null,
        status: "failed",
        plaintextBytes: null,
        cipherBytes: null,
        manifestSigned: false,
        lastVerifyLevel: null,
        lastVerifyResult: null,
        artifacts: [
          {
            destName: "local-nas",
            destKind: "local",
            immutability: "none",
            retentionUntil: null,
            status: "failed",
          },
        ],
      },
    },
  ],
  recent: [],
};
MOCK_DASHBOARD.recent = MOCK_DASHBOARD.jobs
  .map((j) => j.lastBackup)
  .filter((b): b is NonNullable<typeof b> => b !== null);
