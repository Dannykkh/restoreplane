# ADR-004: Recovery Point and Artifact Replica Semantics

- 상태: 승인됨
- 날짜: 2026-07-26

## Context

PostgreSQL globals, DB dump, 다중 Destination을 단일 “Snapshot 성공”으로 표현하면 incomplete
part와 partial destination success가 복원 후보로 노출될 수 있다.

## Decision

```text
Plan Occurrence
  -> Backup Run(one Database Target)
     -> Recovery Point
        -> Artifact Part[]
        -> Artifact Replica[] per Destination
        -> Verification Record[]
```

- Recovery Point는 required Artifact Part 전체와 signed set Manifest가 commit된 원자 단위다.
- Snapshot은 complete Recovery Point의 UI/read projection이다.
- Destination membership은 required 또는 best-effort다.
- 기본 내구성 정책은 `all-required`다.
- Backup Run 성공과 Protection Compliance를 별도로 계산한다.
- incomplete part와 Orphan Artifact는 진단·repair에만 노출한다.

## Consequences

목적지별 retry는 encoded artifact를 재사용할 수 있다. 복원은 complete Recovery Point와
적격한 Artifact Replica만 사용한다.
