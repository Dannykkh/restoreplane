# Research Decision

Date: 2026-11-17. Baseline: main 08ba92615a2585765fa8cb509df37a0671f10454.

## Selected

- Codebase: yes — 기존 service/Operational Store/Workspace/Scope/Tauri 조립점과 테스트를 확인한다.
- Web: yes — 공식 DB 문서의 isolation/취소/transaction 결과, Tauri bounded streaming, SQL 데이터 편집의 안전 계약만 조사한다. 신규 기술 조사 주제는 세 범주로 제한한다.
- GitHub: yes — 사용자가 지정한 ShayanAbbas1/dbdelve commit 307fe2433c2087672651aaa7c12a39c6af418407, stable v0.7.3와 열린 PR을 구분한다. 현재 대화의 소스 조사와 실제 cargo-audit 결과를 재사용한다.
- Competitors: yes — 별도 시장조사 없이 DBDelve의 정보 구조·사용자 흐름을 github 조사에 함께 기록한다.
- Academic: no — 기존 DB 트랜잭션·pagination·backpressure를 사용하는 제품 확장으로 신규 알고리즘 논문이 필요하지 않다.

## Assumptions

- [explicit] Tauri 유지; UI와 Rust 사이 대량 전체 결과 전송을 피한다.
- [explicit] 페이징·인젝션·멱등성·분산 트랜잭션·중복 메시지·데이터 일관성은 필수 설계 계약이다.
- [inferred] Windows RC, 기존 지원 엔진 네 종류를 첫 구현 대상으로 유지한다.
- [inferred] 범용 DBA 권한·계정관리와 모든 DBDelve 엔진의 동시 지원은 후순위다.
- [inferred] DBDelve는 기능·아키텍처 참고이며 GPL 소스 도입은 하지 않는다.

## Questions Deferred

- Q1(선호, 비차단): 첫 완성 범위 — 조회/SQL/편집+백업 연계 권장, 질문 전송됨.
- 운영 데이터·서버 자격증명·클라우드 비용·실제 배포는 이 설계 작업에 필요하지 않다.
