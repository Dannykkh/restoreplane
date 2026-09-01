use restoreplane_catalog::{ActivityEventSpec, AttentionObservation, OperationalStore};
use tempfile::TempDir;

#[test]
fn same_timestamp_activity_uses_sequence_as_a_deterministic_cursor() {
    let fixture = Fixture::new();
    let repository = fixture.store.activity();

    let sequences = repository
        .append_batch(vec![
            ActivityEventSpec::info(
                "event-a",
                1_000,
                "BACKUP_STARTED",
                "backup_run",
                "run-a",
                "started",
            ),
            ActivityEventSpec::info(
                "event-b",
                1_000,
                "BACKUP_PROGRESS",
                "backup_run",
                "run-a",
                "progress",
            ),
            ActivityEventSpec::error(
                "event-c",
                1_000,
                "BACKUP_FAILED",
                "backup_run",
                "run-a",
                "failed",
            ),
        ])
        .expect("append bounded activity batch");
    assert_eq!(sequences.len(), 3);
    assert!(sequences[0] < sequences[1] && sequences[1] < sequences[2]);

    let recent = repository.recent(3).expect("query recent activity");
    let ids: Vec<_> = recent.iter().map(|event| event.id()).collect();
    assert_eq!(ids, vec!["event-c", "event-b", "event-a"]);
    assert_eq!(recent[0].occurred_at_ms(), 1_000);
    assert!(recent[0].seq() > recent[1].seq());
}

#[test]
fn attention_deduplicates_only_active_fingerprint_and_tracks_lifecycle() {
    let fixture = Fixture::new();
    let repository = fixture.store.activity();
    let event_sequences = repository
        .append_batch(vec![
            ActivityEventSpec::error(
                "failure-a",
                2_000,
                "BACKUP_FAILED",
                "backup_run",
                "run-a",
                "failed once",
            ),
            ActivityEventSpec::error(
                "failure-b",
                2_100,
                "BACKUP_FAILED",
                "backup_run",
                "run-a",
                "failed twice",
            ),
        ])
        .expect("append failure events");

    let first = repository
        .observe_attention(AttentionObservation::run_failure(
            "run-a:BACKUP_FAILED",
            "backup_run",
            "run-a",
            "Backup failed",
            event_sequences[0],
            2_000,
        ))
        .expect("open attention");
    let repeated = repository
        .observe_attention(AttentionObservation::run_failure(
            "run-a:BACKUP_FAILED",
            "backup_run",
            "run-a",
            "Backup failed again",
            event_sequences[1],
            2_100,
        ))
        .expect("deduplicate active attention");
    assert_eq!(first.id(), repeated.id());
    assert_eq!(repeated.occurrence_count(), 2);
    assert_eq!(repeated.state(), "open");

    repository
        .acknowledge_attention(first.id(), 2_200)
        .expect("acknowledge attention");
    repository
        .resolve_attention(first.id(), "operator_resolved", 2_300)
        .expect("resolve attention");

    let reopened = repository
        .observe_attention(AttentionObservation::run_failure(
            "run-a:BACKUP_FAILED",
            "backup_run",
            "run-a",
            "Backup failed after resolution",
            event_sequences[1],
            2_400,
        ))
        .expect("open a new attention after terminal resolution");
    assert_ne!(first.id(), reopened.id());
    assert_eq!(reopened.occurrence_count(), 1);
    assert_eq!(
        repository
            .attention_state(first.id())
            .expect("old attention state"),
        "resolved"
    );
}

struct Fixture {
    _temp_dir: TempDir,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let store = OperationalStore::open(temp_dir.path().join("operational.db"))
            .expect("open operational store");
        Self {
            _temp_dir: temp_dir,
            store,
        }
    }
}
