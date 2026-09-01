use restoreplane_dest_ssh::manifest_relative_key;

#[test]
fn only_manifest_paths_inside_the_remote_root_become_relative_keys() {
    assert_eq!(
        manifest_relative_key("/srv/backups/", "/srv/backups/job-a/a.manifest.json"),
        Some("job-a/a.manifest.json".to_string())
    );
    assert_eq!(
        manifest_relative_key(
            "/srv/backups/",
            "/srv/backups/v2/occurrence/target/recovery-point/set-manifest.json"
        ),
        Some("v2/occurrence/target/recovery-point/set-manifest.json".to_string())
    );
    assert_eq!(
        manifest_relative_key("/srv/backups", "/srv/backups/job-a/a.spool"),
        None
    );
    assert_eq!(
        manifest_relative_key("/srv/backups", "/srv/backups-escape/a.manifest.json"),
        None
    );
}
