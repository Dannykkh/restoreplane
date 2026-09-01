# G0 Cutover Recovery Matrix

G1의 아홉 증거가 같은 source revision과 build ID에서 모두 통과하기 전에는 shadow import와
diff report만 허용한다. scheduler freeze, production service execution, cutover marker,
legacy path disable은 금지한다.

| 장애 시점 | 허용 조치 | 금지 조치 | 최종 authority |
|---|---|---|---|
| G1 evidence 전 | rehearsal 중단, legacy 유지 | scheduler freeze, marker 기록 | legacy |
| shadow diff 실패 | shadow DB 폐기, 수정 후 재시도 | import 일부를 production으로 사용 | legacy |
| scheduler freeze 후 marker 전 | service 중지, legacy task enablement 복원 | 두 경로 동시 enable | legacy |
| marker 후 새 v2 데이터 전 | 검증된 recovery bundle로 marker와 scheduler 상태 원복 | 임의 DB 파일 교체 | legacy |
| 첫 v2 control/run 데이터 후 | service authority 유지, export/import 또는 forward-fix | 구 binary downgrade | service |
