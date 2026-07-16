// dbbackup-catalog — S08: SQLite 로컬 캐시(신뢰 불가) + 서명 매니페스트 rebuild/게이트.
//
// 설계 전제(H5/P0-6): 백업 호스트가 랜섬웨어에 장악돼도 오프사이트 단독 복원이 가능해야 한다.
// → 로컬 SQLite는 조회 UX용 캐시일 뿐이며, 복원/검증 판단은 목적지에 동봉된 ed25519 서명
//   매니페스트를 다시 읽어 재검증해서 내린다(gate.rs). 카탈로그가 소실돼도 `catalog rebuild`가
//   목적지 매니페스트만으로 전체 이력을 재구성한다(rebuild.rs).
//
// 매니페스트/서명은 dbbackup-pipeline의 Manifest/SignedManifest(ed25519)를 그대로 재사용한다
// (병렬 매니페스트 타입을 새로 만들지 않는다).

// verify 등 하위 크레이트가 rusqlite 타입을 직접 의존하지 않고 쓰도록 재노출한다.
pub use rusqlite;

pub mod conn;
pub mod gate;
pub mod overlap;
pub mod rebuild;
pub mod source;
pub mod upsert;
pub mod writer;

pub use conn::{open_reader_conn, open_writer_conn, SCHEMA_SQL};
pub use gate::{
    load_and_verify_manifest_for_restore, verify_bytes, EmbeddedKeyVerifier, GateError,
    ManifestVerifier, PinnedKeyVerifier,
};
pub use overlap::{try_acquire_run, OverlapError, RunGuard, MAX_RUN_SECONDS};
pub use rebuild::{catalog_rebuild, RebuildArgs, RebuildReport};
pub use source::{LocalManifestSource, ManifestSource};
pub use upsert::{upsert_backup_from_manifest, upsert_backup_from_manifest_conn};
pub use writer::CatalogWriterHandle;
