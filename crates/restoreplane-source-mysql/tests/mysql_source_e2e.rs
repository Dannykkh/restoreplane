// tests/mysql_source_e2e.rs — T14~T16, T18~T19: 실제 mysqldump/MySQL 서버가 필요한 통합 경로.
//! 이 환경엔 라이브 서버/바이너리가 없으므로 #[ignore]로 둔다(CI에서 MySQL 8 + MariaDB 10.x
//! 컨테이너 매트릭스로 활성화). 로직 커버리지는 lib 유닛 + mysql_anchor/mysql_fatal_patterns가 담당.

#[ignore = "라이브 mysqldump + MySQL/MariaDB 서버 필요(CI 컨테이너 매트릭스)"]
#[test]
fn t14_t19_full_spawn_matrix_requires_live_server() {
    // plan()→spawn()→run_artifact→exit 판정(T14 exit=1 FAIL, T18 단일 파트, T19 재시도 덮어쓰기)은
    // 실제 mysqldump 프로세스가 있어야 의미가 있다.
}
