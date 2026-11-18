import { useEffect, useState } from "react";
import { nativeU64, type Column, type EditableCell } from "../../lib/workbenchTypes";

const KIND_LABEL: Record<EditableCell["kind"], string> = { null: "SQL NULL", text: "텍스트", bool: "참·거짓", i64: "부호 있는 정수", u64: "부호 없는 정수", decimal: "정밀 소수", float: "부동소수점 원문", bytes: "바이트 · Base64", temporal: "날짜·시간 원문" };
export function parseEditValue(kind: EditableCell["kind"], text: string, column: Column, basis?: EditableCell): EditableCell {
  if (new TextEncoder().encode(text).byteLength > 64 * 1024) throw new Error("한 값은 UTF-8 64KiB까지 편집할 수 있습니다.");
  switch (kind) {
    case "null": if (!column.nullable) throw new Error("이 열은 SQL NULL을 허용하지 않습니다."); return { kind: "null" };
    case "text": return { kind, value: text };
    case "bool": if (!["true", "false"].includes(text)) throw new Error("참 또는 거짓을 선택하세요."); return { kind, value: text === "true" };
    case "i64": if (!/^(0|-?[1-9]\d*)$/.test(text) || BigInt(text) < -9223372036854775808n || BigInt(text) > 9223372036854775807n) throw new Error("64비트 정수의 정확한 원문을 입력하세요."); return { kind, value: text };
    case "u64": try { nativeU64(text); } catch { throw new Error("부호 없는 64비트 정수의 정확한 원문을 입력하세요."); } return { kind, value: text };
    case "decimal": if (!/^-?(0|[1-9]\d*)(\.\d+)?$/.test(text)) throw new Error("정밀 소수는 지수 표기 없이 원문 그대로 입력하세요."); return { kind, value: text, ...(basis?.kind === "decimal" && basis.scale !== undefined ? { scale: basis.scale } : {}) };
    case "float": if (!/^(NaN|[-+]?Infinity|[-+]?(\d+(\.\d*)?|\.\d+)([eE][-+]?\d+)?)$/.test(text)) throw new Error("부동소수점 원문 또는 NaN·Infinity를 입력하세요."); return { kind, value: text };
    case "bytes": if (text.length % 4 !== 0 || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(text)) throw new Error("유효한 Base64 바이트를 입력하세요."); return { kind, base64: text };
    case "temporal": if (basis?.kind !== "temporal") throw new Error("날짜·시간의 형식·정밀도·오프셋 기준이 없어 이 입력은 읽기 전용입니다."); return { ...basis, value: text };
  }
}
export function EditCellInput({ column, basis, onValue, disabled }: { column: Column; basis?: EditableCell; onValue: (value: EditableCell) => void; disabled: boolean }) {
  const [kind, setKind] = useState<EditableCell["kind"]>(basis?.kind ?? "text"), [text, setText] = useState(""), [error, setError] = useState("");
  useEffect(() => { setKind(basis?.kind ?? "text"); setText(basis?.kind === "bytes" ? basis.base64 : basis && "value" in basis ? String(basis.value) : ""); setError(""); }, [column.columnId, basis]);
  return <fieldset className="wb-edit-cell" disabled={disabled}><legend>{column.name} · {column.typeName}</legend>
    <label>값 형식<select value={kind} onChange={(event) => { setKind(event.currentTarget.value as EditableCell["kind"]); setError(""); }}>{Object.entries(KIND_LABEL).map(([value, label]) => <option key={value} value={value} disabled={value === "null" && !column.nullable || value === "temporal" && basis?.kind !== "temporal"}>{label}</option>)}</select></label>
    {kind === "bool" ? <label>참·거짓<select value={text} onChange={(event) => setText(event.currentTarget.value)}><option value="">선택하세요</option><option value="true">참</option><option value="false">거짓</option></select></label> : kind !== "null" && <label>새 값 원문<textarea value={text} spellCheck={false} maxLength={65536} onChange={(event) => setText(event.currentTarget.value)} /></label>}
    {kind === "text" && <small>빈 입력은 빈 문자열입니다. SQL NULL·서버 기본값과 구분합니다.</small>}
    {kind === "temporal" && basis?.kind === "temporal" && <small>형식 {basis.typeName} · 정밀도 {basis.precision} · 오프셋 {basis.offset ?? "없음"}을 유지합니다.</small>}
    <button className="button button-secondary" type="button" onClick={() => { try { onValue(parseEditValue(kind, text, column, basis)); setError(""); } catch (failure) { setError(failure instanceof Error ? failure.message : "값 형식을 확인하세요."); } }}>이 값을 변경 초안에 추가</button>
    {error && <p className="wb-error" role="alert">{error}</p>}
  </fieldset>;
}
