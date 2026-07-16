import { useState } from "react";
import "./Setup.css";
import { initKeystore, type InitResult } from "../lib/api";
import { IconCheck, IconLock, IconShield } from "../components/Icons";

type Step = 1 | 2 | 3 | 4;
const STEP_LABELS = ["소개", "마스터 비밀번호", "복구코드", "완료"];

export function Setup({ onComplete }: { onComplete: () => void }) {
  const [step, setStep] = useState<Step>(1);
  const [pw, setPw] = useState("");
  const [pw2, setPw2] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [result, setResult] = useState<InitResult | null>(null);
  const [acked, setAcked] = useState(false);
  const [copied, setCopied] = useState(false);

  const pwError =
    pw.length > 0 && pw.length < 8
      ? "8자 이상이어야 합니다."
      : pw2.length > 0 && pw !== pw2
        ? "비밀번호가 일치하지 않습니다."
        : null;
  const canSubmitPw = pw.length >= 8 && pw === pw2 && !busy;

  async function runInit() {
    setBusy(true);
    setErr(null);
    try {
      const res = await initKeystore(pw);
      setResult(res);
      setPw("");
      setPw2("");
      setStep(3);
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  }

  async function copyCode() {
    if (!result) return;
    try {
      await navigator.clipboard.writeText(result.recoveryCode);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      /* 클립보드 불가 환경은 무시(수기 전사) */
    }
  }

  return (
    <div className="wizard">
      <div className="stepper">
        {STEP_LABELS.map((label, i) => {
          const n = (i + 1) as Step;
          const cls = n === step ? "active" : n < step ? "done" : "";
          return (
            <div key={label} style={{ display: "contents" }}>
              <div className={`step-dot ${cls}`}>
                <span className="num">{n < step ? <IconCheck /> : n}</span>
                {label}
              </div>
              {i < STEP_LABELS.length - 1 && <span className="step-line" />}
            </div>
          );
        })}
      </div>

      <div className="wizard-panel">
        {step === 1 && (
          <>
            <h1>백업 신뢰의 뿌리를 만듭니다</h1>
            <p className="wizard-lead">
              마스터 비밀번호로 키를 봉투암호화하고, 백업마다 서명키로 서명합니다. 이 과정은 한 번만
              하면 됩니다.
            </p>
            <ul className="trust-list">
              <li>
                <span className="ti">
                  <IconLock width={16} height={16} />
                </span>
                <div>
                  <div className="tt">3경로 봉투암호화</div>
                  <div className="td">비밀번호 · 복구코드 · DPAPI(무인 실행) 중 하나로 키를 복구합니다.</div>
                </div>
              </li>
              <li>
                <span className="ti">
                  <IconShield width={16} height={16} />
                </span>
                <div>
                  <div className="tt">백업 서명키</div>
                  <div className="td">복원 시 위조·미서명 백업을 복호화 전에 거부합니다.</div>
                </div>
              </li>
              <li>
                <span className="ti">
                  <IconCheck width={16} height={16} />
                </span>
                <div>
                  <div className="tt">복구코드 1회 발급</div>
                  <div className="td">비밀번호를 잊어도 복구코드로 키를 되살릴 수 있습니다.</div>
                </div>
              </li>
            </ul>
            <div className="wizard-actions">
              <span />
              <button className="btn btn-primary" onClick={() => setStep(2)}>
                시작하기
              </button>
            </div>
          </>
        )}

        {step === 2 && (
          <>
            <h1>마스터 비밀번호 설정</h1>
            <p className="wizard-lead">
              이 비밀번호로 키를 잠급니다. 분실 시 다음 단계의 복구코드로만 되살릴 수 있으니 신중히
              정하세요.
            </p>
            <div className="field">
              <label className="label">마스터 비밀번호</label>
              <input
                className="input"
                type="password"
                autoFocus
                value={pw}
                onChange={(e) => setPw(e.target.value)}
                placeholder="8자 이상"
              />
            </div>
            <div className="field">
              <label className="label">비밀번호 확인</label>
              <input
                className="input"
                type="password"
                value={pw2}
                onChange={(e) => setPw2(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && canSubmitPw && runInit()}
              />
              {pwError && <div className="field-error">{pwError}</div>}
              {!pwError && (
                <div className="field-hint">긴 패스프레이즈를 권장합니다(예: 단어 4개 이상).</div>
              )}
            </div>
            {err && <div className="field-error">{err}</div>}
            <div className="wizard-actions">
              <button className="btn btn-ghost" onClick={() => setStep(1)} disabled={busy}>
                뒤로
              </button>
              <button className="btn btn-primary" onClick={runInit} disabled={!canSubmitPw}>
                {busy ? "키 생성 중…" : "키스토어 생성"}
              </button>
            </div>
          </>
        )}

        {step === 3 && result && (
          <>
            <h1>복구코드를 보관하세요</h1>
            <div className="recovery-warn">
              <IconLock width={16} height={16} />
              <span>
                이 코드는 <b>지금 한 번만</b> 표시됩니다. 비밀번호를 잊었을 때 키를 되살릴 유일한
                수단이니, 비밀번호 관리자나 오프라인에 안전하게 보관하세요.
              </span>
            </div>
            <div className="recovery-box">
              <span className="recovery-code">{result.recoveryCode}</span>
              <button className="btn btn-ghost copy-btn" onClick={copyCode}>
                {copied ? "복사됨" : "복사"}
              </button>
            </div>
            <label className="ack">
              <input type="checkbox" checked={acked} onChange={(e) => setAcked(e.target.checked)} />
              복구코드를 안전한 곳에 보관했습니다. 다시 표시되지 않는 것을 이해합니다.
            </label>
            <div className="wizard-actions">
              <span />
              <button className="btn btn-primary" onClick={() => setStep(4)} disabled={!acked}>
                계속
              </button>
            </div>
          </>
        )}

        {step === 4 && result && (
          <>
            <div className="done-mark">
              <IconCheck width={26} height={26} />
            </div>
            <h1>설정 완료</h1>
            <p className="wizard-lead">
              키스토어와 서명키가 생성되었습니다. 이제 DB와 목적지를 등록하고 백업을 실행할 수
              있습니다.
            </p>
            <div className="done-key">
              <div className="k">서명 공개키(fingerprint 대조용)</div>
              <div className="v">{result.verifyingKeyHex}</div>
            </div>
            <div className="wizard-actions">
              <span />
              <button className="btn btn-primary" onClick={onComplete}>
                대시보드로 이동
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
