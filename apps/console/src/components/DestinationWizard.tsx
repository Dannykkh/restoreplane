import { useState, type FormEvent } from "react";
import {
  getWorkItem,
  probeDestination,
  saveDestination,
} from "../lib/api";
import { asServiceError, ServiceError } from "../lib/errors";
import { useMutationAttemptRegistry } from "../lib/mutationAttempts";
import type { DestinationDraft } from "../lib/types";
import { GuidedWorkspace } from "./GuidedWorkspace";
import { PageState } from "./PageState";
import { StatusBadge } from "./StatusBadge";

const STEPS = ["Endpoint", "Role credentials", "Probe evidence"] as const;

type DestinationKind = DestinationDraft["kind"];

interface DestinationWizardProps {
  onCancel: () => void;
  onComplete: (destinationId: string) => void;
}

interface FormState {
  displayName: string;
  kind: DestinationKind;
  localPath: string;
  bucket: string;
  prefix: string;
  region: string;
  endpointUrl: string;
  forcePathStyle: boolean;
  provider: "aws" | "minio" | "backblaze_b2" | "cloudflare_r2";
  objectLockMode: "none" | "governance" | "compliance";
  retentionDays: number;
  minDwellDays: number;
  complianceOptIn: boolean;
  sshHost: string;
  sshPort: number;
  sshUser: string;
  sshPath: string;
  hostKeyFingerprint: string;
  appendOnly: boolean;
  writerIdentity: string;
  writerSecret: string;
  readerIdentity: string;
  readerSecret: string;
  deleteIdentity: string;
  deleteSecret: string;
}

const initialForm: FormState = {
  displayName: "",
  kind: "local",
  localPath: "",
  bucket: "",
  prefix: "",
  region: "us-east-1",
  endpointUrl: "",
  forcePathStyle: false,
  provider: "aws",
  objectLockMode: "none",
  retentionDays: 0,
  minDwellDays: 0,
  complianceOptIn: false,
  sshHost: "",
  sshPort: 22,
  sshUser: "",
  sshPath: "",
  hostKeyFingerprint: "",
  appendOnly: false,
  writerIdentity: "",
  writerSecret: "",
  readerIdentity: "",
  readerSecret: "",
  deleteIdentity: "",
  deleteSecret: "",
};

export function DestinationWizard({
  onCancel,
  onComplete,
}: DestinationWizardProps) {
  const mutationAttempts = useMutationAttemptRegistry();
  const [step, setStep] = useState(0);
  const [form, setForm] = useState(initialForm);
  const [destinationId, setDestinationId] = useState<string | null>(null);
  const [workItemId, setWorkItemId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ServiceError | null>(null);

  function update<K extends keyof FormState>(
    key: K,
    value: FormState[K],
  ): void {
    setForm((current) => ({ ...current, [key]: value }));
  }

  function submitEndpoint(event: FormEvent): void {
    event.preventDefault();
    setError(null);
    setStep(1);
  }

  async function saveAndProbe(event: FormEvent): Promise<void> {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const draft = buildDraft(form);
      const id =
        destinationId ??
        (await mutationAttempts.run(
          "destination-save",
          "destination.save",
          draft,
          (key) => saveDestination(draft, key),
        )).destinationId;
      setDestinationId(id);
      clearSecrets();
      const work = await mutationAttempts.run(
        "destination-probe",
        "destination.probe",
        { destinationId: id },
        (key) => probeDestination(id, key),
      );
      setWorkItemId(work.workItemId);
      if (!isSuccessful(work.state)) {
        await waitForSuccessfulWork(work.workItemId);
      }
      setStep(2);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  function clearSecrets(): void {
    setForm((current) => ({
      ...current,
      writerSecret: "",
      readerSecret: "",
      deleteSecret: "",
    }));
  }

  return (
    <GuidedWorkspace
      title="새 Destination"
      description="저장 endpoint와 writer, reader, delete 권한을 분리하고 실제 객체로 검증합니다."
      steps={STEPS}
      currentStep={step}
      footer={
        <>
          <button
            type="button"
            className="button button-secondary"
            disabled={busy}
            onClick={onCancel}
          >
            취소
          </button>
          {step === 1 && (
            <button
              type="submit"
              form="destination-role-form"
              className="button button-primary"
              disabled={busy}
            >
              {busy ? "역할 Probe 실행 중" : "저장 후 역할 Probe"}
            </button>
          )}
          {step === 2 && destinationId && (
            <button
              type="button"
              className="button button-primary"
              onClick={() => onComplete(destinationId)}
            >
              Destination 보기
            </button>
          )}
        </>
      }
    >
      <div className="live-region" aria-live="polite">
        {busy
          ? "서비스가 역할별 임시 객체 검사를 수행하고 있습니다."
          : workItemId
            ? `Work Item ${workItemId}`
            : ""}
      </div>
      {error && (
        <PageState
          kind="error"
          title={error.message}
          detail="저장 성공과 writer 성공만으로 복구 가능한 Destination으로 표시하지 않습니다. endpoint와 실패한 역할의 자격 증명을 확인하세요."
          error={error}
        />
      )}

      {step === 0 && (
        <form className="form-stack" onSubmit={submitEndpoint}>
          <p className="form-intro">
            Endpoint 정보는 카탈로그에 저장됩니다. 자격 증명은 다음 단계에서
            역할별 Secret Store로 직접 전달됩니다.
          </p>
          <div className="form-grid">
            <label className="field">
              <span>Destination 이름</span>
              <input
                required
                name="destinationDisplayName"
                value={form.displayName}
                onChange={(event) =>
                  update("displayName", event.target.value)
                }
                autoComplete="off"
              />
            </label>
            <label className="field">
              <span>저장소 종류</span>
              <select
                name="destinationKind"
                value={form.kind}
                onChange={(event) =>
                  update("kind", event.target.value as DestinationKind)
                }
              >
                <option value="local">Local filesystem</option>
                <option value="s3">S3 compatible</option>
                <option value="ssh_sftp">SSH/SFTP</option>
              </select>
            </label>
          </div>

          {form.kind === "local" && (
            <div className="field">
              <label htmlFor="destination-local-path">절대 경로</label>
              <input
                id="destination-local-path"
                required
                name="localPath"
                value={form.localPath}
                onChange={(event) => update("localPath", event.target.value)}
                placeholder="D:\RestorePlane\vault"
                autoComplete="off"
                aria-describedby="destination-local-path-help"
              />
              <small id="destination-local-path-help">
                Windows 서비스 계정이 쓰기, 읽기, 삭제할 수 있는 절대 경로여야
                합니다.
              </small>
            </div>
          )}

          {form.kind === "s3" && (
            <>
              <div className="form-grid">
                <label className="field">
                  <span>Bucket</span>
                  <input
                    required
                    name="s3Bucket"
                    value={form.bucket}
                    onChange={(event) => update("bucket", event.target.value)}
                    autoComplete="off"
                  />
                </label>
                <label className="field">
                  <span>Prefix</span>
                  <input
                    name="s3Prefix"
                    value={form.prefix}
                    onChange={(event) => update("prefix", event.target.value)}
                    autoComplete="off"
                  />
                </label>
              </div>
              <div className="form-grid">
                <label className="field">
                  <span>Provider</span>
                  <select
                    name="s3Provider"
                    value={form.provider}
                    onChange={(event) =>
                      update(
                        "provider",
                        event.target.value as FormState["provider"],
                      )
                    }
                  >
                    <option value="aws">AWS S3</option>
                    <option value="minio">MinIO</option>
                    <option value="backblaze_b2">Backblaze B2</option>
                    <option value="cloudflare_r2">Cloudflare R2</option>
                  </select>
                </label>
                <label className="field">
                  <span>Region</span>
                  <input
                    required
                    name="s3Region"
                    value={form.region}
                    onChange={(event) => update("region", event.target.value)}
                    autoComplete="off"
                  />
                </label>
              </div>
              <label className="field">
                <span>Endpoint URL</span>
                <input
                  name="s3EndpointUrl"
                  type="url"
                  value={form.endpointUrl}
                  onChange={(event) =>
                    update("endpointUrl", event.target.value)
                  }
                  placeholder="https://s3.example.internal"
                  autoComplete="off"
                />
                <small>AWS 기본 endpoint를 사용하면 비워 둡니다.</small>
              </label>
              <div className="form-grid">
                <label className="field">
                  <span>Object Lock</span>
                  <select
                    name="objectLockMode"
                    value={form.objectLockMode}
                    onChange={(event) =>
                      update(
                        "objectLockMode",
                        event.target.value as FormState["objectLockMode"],
                      )
                    }
                  >
                    <option value="none">None</option>
                    <option value="governance">Governance</option>
                    <option value="compliance">Compliance</option>
                  </select>
                </label>
                <label className="field">
                  <span>보존 일수</span>
                  <input
                    name="retentionDays"
                    type="number"
                    min={0}
                    max={36500}
                    value={form.retentionDays}
                    onChange={(event) =>
                      update("retentionDays", Number(event.target.value))
                    }
                  />
                </label>
              </div>
              <fieldset className="choice-fieldset">
                <legend>S3 endpoint 옵션</legend>
                <label>
                  <input
                    type="checkbox"
                    name="forcePathStyle"
                    checked={form.forcePathStyle}
                    onChange={(event) =>
                      update("forcePathStyle", event.target.checked)
                    }
                  />
                  <span>Path-style endpoint 사용</span>
                </label>
                {form.objectLockMode === "compliance" && (
                  <label>
                    <input
                      required
                      type="checkbox"
                      name="complianceOptIn"
                      checked={form.complianceOptIn}
                      onChange={(event) =>
                        update("complianceOptIn", event.target.checked)
                      }
                    />
                    <span>
                      Compliance 잠금의 비가역성과 보존 기간을 확인했습니다.
                    </span>
                  </label>
                )}
              </fieldset>
            </>
          )}

          {form.kind === "ssh_sftp" && (
            <>
              <div className="form-grid form-grid-endpoint">
                <label className="field">
                  <span>SSH 호스트</span>
                  <input
                    required
                    name="sshHost"
                    value={form.sshHost}
                    onChange={(event) => update("sshHost", event.target.value)}
                    autoComplete="off"
                  />
                </label>
                <label className="field">
                  <span>포트</span>
                  <input
                    required
                    name="sshPort"
                    type="number"
                    min={1}
                    max={65535}
                    value={form.sshPort}
                    onChange={(event) =>
                      update("sshPort", Number(event.target.value))
                    }
                  />
                </label>
              </div>
              <div className="form-grid">
                <label className="field">
                  <span>SSH 사용자</span>
                  <input
                    required
                    name="sshUser"
                    value={form.sshUser}
                    onChange={(event) => update("sshUser", event.target.value)}
                    autoComplete="off"
                  />
                </label>
                <label className="field">
                  <span>원격 경로</span>
                  <input
                    required
                    name="sshPath"
                    value={form.sshPath}
                    onChange={(event) => update("sshPath", event.target.value)}
                    placeholder="/srv/restoreplane"
                    autoComplete="off"
                  />
                </label>
              </div>
              <label className="field">
                <span>Host key SHA-256 fingerprint</span>
                <input
                  required
                  name="hostKeyFingerprint"
                  value={form.hostKeyFingerprint}
                  onChange={(event) =>
                    update("hostKeyFingerprint", event.target.value)
                  }
                  placeholder="SHA256:..."
                  autoComplete="off"
                />
                <small>
                  첫 연결 자동 신뢰는 허용하지 않습니다. 운영자가 확인한 fingerprint를
                  고정합니다.
                </small>
              </label>
              <fieldset className="choice-fieldset">
                <legend>서버 보존 정책</legend>
                <label>
                  <input
                    type="checkbox"
                    name="appendOnly"
                    checked={form.appendOnly}
                    onChange={(event) =>
                      update("appendOnly", event.target.checked)
                    }
                  />
                  <span>서버가 append-only 정책을 강제함</span>
                </label>
              </fieldset>
            </>
          )}

          <div className="form-submit">
            <button type="submit" className="button button-primary">
              역할 자격 증명
            </button>
          </div>
        </form>
      )}

      {step === 1 && (
        <form
          id="destination-role-form"
          className="form-stack"
          onSubmit={(event) => void saveAndProbe(event)}
        >
          {form.kind === "local" ? (
            <div className="probe-stage">
              <StatusBadge label="Service identity" tone="neutral" />
              <h3>Local 경로는 별도 비밀을 저장하지 않습니다.</h3>
              <p>
                서비스 계정으로 임시 객체를 쓰고, BLAKE3 해시를 읽어 확인한 뒤
                삭제합니다. 세 역할의 결과는 각각 기록됩니다.
              </p>
            </div>
          ) : (
            <>
              <p className="form-intro">
                writer와 reader는 반드시 서로 독립적으로 검사합니다. delete
                credential은 보존 작업을 허용할 때만 입력하세요.
              </p>
              <RoleCredentialFields
                kind={form.kind}
                roleLabel="Backup writer"
                identity={form.writerIdentity}
                secret={form.writerSecret}
                required
                onIdentity={(value) => update("writerIdentity", value)}
                onSecret={(value) => update("writerSecret", value)}
              />
              <RoleCredentialFields
                kind={form.kind}
                roleLabel="Restore reader"
                identity={form.readerIdentity}
                secret={form.readerSecret}
                required
                onIdentity={(value) => update("readerIdentity", value)}
                onSecret={(value) => update("readerSecret", value)}
              />
              <RoleCredentialFields
                kind={form.kind}
                roleLabel="Delete operator"
                identity={form.deleteIdentity}
                secret={form.deleteSecret}
                onIdentity={(value) => update("deleteIdentity", value)}
                onSecret={(value) => update("deleteSecret", value)}
              />
            </>
          )}
        </form>
      )}

      {step === 2 && (
        <div className="ready-summary">
          <StatusBadge label="Role probes passed" tone="success" />
          <h3>{form.displayName}의 쓰기와 복구 읽기 근거가 확인됐습니다.</h3>
          <p>
            임시 객체의 writer commit과 reader BLAKE3 검사가 모두 통과했습니다.
            delete 역할은 구성된 경우에만 별도로 검사됐습니다.
          </p>
          <dl>
            <div>
              <dt>Destination ID</dt>
              <dd className="mono">{destinationId}</dd>
            </div>
            <div>
              <dt>Work Item</dt>
              <dd className="mono">{workItemId}</dd>
            </div>
          </dl>
        </div>
      )}
    </GuidedWorkspace>
  );
}

interface RoleCredentialFieldsProps {
  kind: "s3" | "ssh_sftp";
  roleLabel: string;
  identity: string;
  secret: string;
  required?: boolean;
  onIdentity: (value: string) => void;
  onSecret: (value: string) => void;
}

function RoleCredentialFields({
  kind,
  roleLabel,
  identity,
  secret,
  required = false,
  onIdentity,
  onSecret,
}: RoleCredentialFieldsProps) {
  return (
    <fieldset className="choice-fieldset">
      <legend>{roleLabel}</legend>
      <div className="form-grid">
        {kind === "s3" && (
          <label className="field">
            <span>{roleLabel} access key ID</span>
            <input
              required={required || secret.length > 0}
              name={`${roleLabel.replaceAll(" ", "-").toLowerCase()}Identity`}
              value={identity}
              onChange={(event) => onIdentity(event.target.value)}
              autoComplete="off"
            />
          </label>
        )}
        <label className="field">
          <span>
            {roleLabel} {kind === "s3" ? "secret access key" : "password"}
          </span>
          <input
            required={required || identity.length > 0}
            name={`${roleLabel.replaceAll(" ", "-").toLowerCase()}Secret`}
            type="password"
            value={secret}
            onChange={(event) => onSecret(event.target.value)}
            autoComplete="new-password"
          />
        </label>
      </div>
    </fieldset>
  );
}

function buildDraft(form: FormState): DestinationDraft {
  if (form.kind === "local") {
    return {
      displayName: form.displayName,
      kind: "local",
      endpoint: { path: form.localPath },
      credentials: {},
    };
  }
  if (form.kind === "s3") {
    return {
      displayName: form.displayName,
      kind: "s3",
      endpoint: {
        bucket: form.bucket,
        prefix: form.prefix,
        region: form.region,
        ...(form.endpointUrl ? { endpointUrl: form.endpointUrl } : {}),
        forcePathStyle: form.forcePathStyle,
        provider: form.provider,
        objectLockMode: form.objectLockMode,
        retentionDays: form.retentionDays,
        minDwellDays: form.minDwellDays,
        ...(form.objectLockMode === "none"
          ? {}
          : { checksumAlgo: "crc32c" }),
        complianceOptIn: form.complianceOptIn,
      },
      credentials: {
        backupWriter: {
          schemaVersion: 1,
          accessKeyId: form.writerIdentity,
          secretAccessKey: form.writerSecret,
        },
        restoreReader: {
          schemaVersion: 1,
          accessKeyId: form.readerIdentity,
          secretAccessKey: form.readerSecret,
        },
        ...(form.deleteIdentity && form.deleteSecret
          ? {
              deleteOperator: {
                schemaVersion: 1 as const,
                accessKeyId: form.deleteIdentity,
                secretAccessKey: form.deleteSecret,
              },
            }
          : {}),
      },
    };
  }
  return {
    displayName: form.displayName,
    kind: "ssh_sftp",
    endpoint: {
      host: form.sshHost,
      port: form.sshPort,
      user: form.sshUser,
      path: form.sshPath,
      hostKeyFingerprint: form.hostKeyFingerprint,
      appendOnly: form.appendOnly,
    },
    credentials: {
      backupWriter: sshPasswordCredential(form.writerSecret),
      restoreReader: sshPasswordCredential(form.readerSecret),
      ...(form.deleteSecret
        ? { deleteOperator: sshPasswordCredential(form.deleteSecret) }
        : {}),
    },
  };
}

function sshPasswordCredential(password: string) {
  return {
    schemaVersion: 1 as const,
    auth: { kind: "password" as const, password },
  };
}

function isSuccessful(state: string): boolean {
  return state === "succeeded" || state === "partially_succeeded";
}

async function waitForSuccessfulWork(workItemId: string) {
  for (let attempt = 0; attempt < 120; attempt += 1) {
    const work = await getWorkItem(workItemId);
    if (isSuccessful(work.state)) return work;
    if (
      work.state === "failed_terminal" ||
      work.state === "failed" ||
      work.state === "cancelled"
    ) {
      throw new ServiceError(
        work.errorCode ?? "DESTINATION_PROBE_FAILED",
        { retryable: false },
      );
    }
    await new Promise((resolve) => window.setTimeout(resolve, 500));
  }
  throw new ServiceError("PROCESS_TIMEOUT", { retryable: true });
}
