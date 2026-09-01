# DESIGN.md — RestorePlane 관리 GUI (정본)

> 이 파일은 디자인 정본이다. UI 작업 전 반드시 먼저 읽고 이 토큰(색·타이포·간격·라운드)을 따른다.
> 대상: Tauri 데스크톱 앱(웹 프론트). 방향: **보안 콘솔(다크) + 침착한 틸/블루 트러스트**.
> 정서: 랜섬웨어를 겪은 운영자에게 "내 데이터는 안전하다"를 시각적으로 전달 — 차분함 = 안심.

## 디자인 원칙 (산문)

1. **차분함이 곧 안심이다.** 네온·과채도 경보색으로 불안을 자극하지 않는다. 상태는 또렷하되 절제된 색으로.
   "모든 게 정상"인 상태가 화면의 기본값이고 시각적으로 편안해야 한다.
2. **기술적 진실을 모노스페이스로.** 체크섬, 서버/도구 버전, 오브젝트 키, 용량, 검증 레벨(L1/L2/L3),
   보존기한 같은 값은 JetBrains Mono로 렌더 — 정밀함이 신뢰를 만든다.
3. **불변성·검증 상태는 배지로 명시.** "불변(Object Lock)", "서명 검증됨", "L3 복원 통과"를 아이콘+색 배지로
   즉시 읽히게. 사용자의 근본 불안(복구 가능한가?)에 화면이 매 순간 답한다.
4. **위험 액션은 마찰을 준다.** prune/삭제/복원은 확인 마찰(문구 재입력, 명시적 경고). 안전 액션은 매끄럽게.
5. **비대칭 밀도.** 대시보드는 데이터 밀도, 마법사(init/복원)는 넓은 여백으로 한 번에 한 결정. 균일 카드 그리드 지양.

## 디자인 토큰 (YAML)

```yaml
meta:
  name: restoreplane-console
  preset: { variance: 2, motion: 2, density: 3 }   # 절제된 변주 / 은은한 모션 / 대시보드 밀도
  theme: dark-primary                               # 다크가 기본, light는 override 제공

colors:
  # near-black 슬레이트 베이스 (순수 검정 금지)
  bg:            { value: "#14181F", oklch: "oklch(0.18 0.015 240)" }
  surface:       { value: "#1B212B", oklch: "oklch(0.23 0.018 240)" }
  surface-2:     { value: "#232B37", oklch: "oklch(0.27 0.020 240)" }
  border:        { value: "#2E3846", oklch: "oklch(0.32 0.020 240)" }
  border-strong: { value: "#3B4757", oklch: "oklch(0.38 0.022 240)" }

  text:          { value: "#EDF1F6", oklch: "oklch(0.95 0.008 240)" }   # on bg/surface
  text-muted:    { value: "#A5B0BE", oklch: "oklch(0.73 0.020 240)" }   # 보조 텍스트
  text-faint:    { value: "#6C7788", oklch: "oklch(0.55 0.020 240)" }   # 캡션/디스에이블

  # 지배색: 침착한 틸 트러스트
  primary:       { value: "#3FBFB0", oklch: "oklch(0.74 0.095 190)" }
  primary-strong:{ value: "#2AA396", oklch: "oklch(0.66 0.095 190)" }
  on-primary:    { value: "#052420", oklch: "oklch(0.22 0.03 190)" }    # 틸 위 전경(대비 확보)

  # 날카로운 액센트: 포커스/CTA (틸과 인접 계열로 통일감)
  accent:        { value: "#5BC8E8", oklch: "oklch(0.79 0.10 220)" }
  on-accent:     { value: "#04222C", oklch: "oklch(0.22 0.04 220)" }

  # 상태색 (절제된 채도 — 경보가 아니라 안내)
  success:       { value: "#46C08A", oklch: "oklch(0.74 0.12 158)" }    # 백업 성공 / L3 통과
  on-success:    { value: "#04241A" }
  warning:       { value: "#E0A94A", oklch: "oklch(0.79 0.11 80)" }     # 검증 대기 / 오래된 백업
  on-warning:    { value: "#2A1D05" }
  danger:        { value: "#E56A62", oklch: "oklch(0.68 0.15 27)" }     # 실패 / 미서명 거부
  on-danger:     { value: "#2A0906" }
  info:          { value: "#3FBFB0" }                                   # = primary

  # 불변성/보안 전용 강조 (배지)
  immutable:     { value: "#7C9CF5", oklch: "oklch(0.72 0.11 265)" }    # Object Lock / 서명
  on-immutable:  { value: "#0A1130" }

typography:
  heading:
    fontFamily: "Space Grotesk, Pretendard, sans-serif"   # 라틴 기하학 + 한글 폴백 동봉(필수)
    fontSize: { h1: "28px", h2: "22px", h3: "18px" }
    fontWeight: 600
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Pretendard, sans-serif"                  # 한글·라틴 모두 커버
    fontSize: { base: "14px", sm: "13px", lg: "16px" }
    fontWeight: 400
    lineHeight: 1.55
  label:
    fontFamily: "Pretendard, sans-serif"
    fontSize: { base: "12px" }
    fontWeight: 500
    letterSpacing: "0.02em"
  mono:                                                    # 기술값 전용
    fontFamily: "JetBrains Mono, ui-monospace, monospace"
    fontSize: { base: "12px", sm: "11px" }
    usage: "checksum, 버전, object key, 용량, 보존기한, 커맨드 프리뷰"

spacing:
  scale: [2, 4, 8, 12, 16, 24, 32, 48, 64]   # px, 4의 배수 기반
  gutter: "16px"
  section: "32px"

rounded:
  sm: "6px"      # 배지, 인풋
  md: "10px"     # 카드, 버튼
  lg: "14px"     # 모달, 마법사 패널
  full: "999px"  # 상태 dot, pill

elevation:
  card: "0 1px 2px rgba(0,0,0,0.30), 0 0 0 1px {colors.border}"
  modal: "0 12px 40px rgba(0,0,0,0.50)"
  focus-ring: "0 0 0 3px color-mix(in oklch, {colors.accent} 45%, transparent)"

motion:
  duration: { fast: "120ms", base: "200ms", slow: "320ms" }
  easing: "cubic-bezier(0.2, 0.0, 0.0, 1.0)"
  rule: "transform/opacity만 애니메이트. prefers-reduced-motion 존중. 상태 전환은 은은하게."

components:
  button-primary:
    bg: "{colors.primary}"
    color: "{colors.on-primary}"
    rounded: "{rounded.md}"
    padding: "8px 16px"
    hover: "{colors.primary-strong}"
  button-danger:
    bg: "transparent"
    color: "{colors.danger}"
    border: "1px solid {colors.danger}"
    note: "위험 액션은 채움 대신 아웃라인 + 확인 마찰"
  card:
    bg: "{colors.surface}"
    border: "1px solid {colors.border}"
    rounded: "{rounded.md}"
    padding: "16px"
    shadow: "{elevation.card}"
  input:
    bg: "{colors.surface-2}"
    color: "{colors.text}"
    border: "1px solid {colors.border-strong}"
    rounded: "{rounded.sm}"
    focus: "{elevation.focus-ring}"
  status-badge:
    rounded: "{rounded.full}"
    padding: "2px 10px"
    variants:
      success:   { bg: "color-mix(in oklch, {colors.success} 18%, transparent)",   color: "{colors.success}" }
      warning:   { bg: "color-mix(in oklch, {colors.warning} 18%, transparent)",   color: "{colors.warning}" }
      danger:    { bg: "color-mix(in oklch, {colors.danger} 18%, transparent)",    color: "{colors.danger}" }
      immutable: { bg: "color-mix(in oklch, {colors.immutable} 18%, transparent)", color: "{colors.immutable}" }
```

## 라이트 테마 override (접근성/선호 대응, 부차)

```yaml
theme-light:
  bg:         "#F5F7FA"
  surface:    "#FFFFFF"
  surface-2:  "#EDF1F6"
  border:     "#D6DEE8"
  text:       "#141A22"
  text-muted: "#4A5566"
  primary:    "#1E9E90"     # 라이트 배경 대비 위해 톤 조정
  on-primary: "#FFFFFF"
# select > option, placeholder, 스크롤바, 자동완성 배경도 테마 토큰 준수. 루트에 color-scheme: dark light.
```

## 폰트 로딩 (검증 필수)

```
Pretendard:     한글 포함 UI라 반드시 폰트 스택에 동봉 + 실제 로드 (@font-face/CDN).
                라틴 페어링(Space Grotesk)엔 한글 글리프 없음 → 스택 동봉 안 하면 한글 시스템 폴백.
Space Grotesk:  헤딩 라틴만. 쓰는 weight(600)만 로드.
JetBrains Mono: 기술값. 400/500만 로드.
검증: document.fonts.check('600 16px "Space Grotesk"') — 단 Pretendard는 unicode-range 서브셋이라
      face status로 확인. 미사용 weight 로드 금지.
```

## 화면별 디자인 의도 (요약)

| 화면 | 밀도 | 핵심 시각 장치 |
|------|------|----------------|
| 초기 셋업 마법사(비번/복구코드/DPAPI/서명키) | 낮음(여백↑) | 한 스텝 한 결정, 복구코드는 1회 표시 경고 모달, 진행 stepper |
| DB·목적지 등록 | 중간 | 연결 테스트 인라인 피드백(성공 dot), 불변성 capability 배지 |
| 백업 현황 대시보드 | 높음 | job별 카드: 마지막 백업 시각 + 검증 레벨 배지 + 불변성 배지 + 다음 예약. 전역 "모두 정상" 안심 헤더 |
| 복원 마법사 | 낮음(여백↑) | 서명 검증 통과 게이트 → 샌드박스 복원 진행 로그, 위험 경고 |
| 스케줄/알림 | 중간 | cron 프리뷰, RPO 한계 명시 배너, 알림 채널 |

## AI Slop 금지 (이 프로젝트 한정)
- 보라/파랑 그라데이션 hero 금지, 균일 16px 라운드 카드 그리드 금지, "Build the future"류 카피 금지.
- 순수 #000 금지(near-black 슬레이트 사용), Inter 기본값 금지(Pretendard 정본).
- 경보색 남발 금지 — 상태색은 배지 배경 18% 틴트로 절제.
