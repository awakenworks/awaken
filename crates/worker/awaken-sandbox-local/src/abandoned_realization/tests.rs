use super::*;

fn identity(inode: u64) -> DirectoryIdentity {
    DirectoryIdentity { device: 7, inode }
}

/// The reclaimable/refused rule, exhausted over the table on
/// [`classify_legacy_root`].
///
/// Causes: C1 the record (absent / in-flight / retained); C2 the observed entry
/// (absent / directory / other file type); C3 the recorded inode against the
/// observed one (unbound / equal / different).
/// Effects: E1 create, E2 reclaim, E3 refuse.
///
/// | Rule | C1        | C2        | C3        | Effect |
/// | R1   | absent    | absent    | n/a       | E1     |
/// | R2   | absent    | any other | n/a       | E3     |
/// | R3   | in-flight | absent    | n/a       | E1     |
/// | R4   | in-flight | directory | unbound   | E2     |
/// | R5   | in-flight | directory | equal     | E2     |
/// | R6   | in-flight | directory | different | E3     |
/// | R7   | in-flight | other     | n/a       | E3     |
/// | R8   | retained  | any       | any       | E3     |
#[test]
fn only_this_authoritys_own_unsettled_creation_may_reclaim_its_root() {
    let bound = LegacyCreationRecord::in_flight(Some(identity(1)));
    let unbound = LegacyCreationRecord::in_flight(None);
    let retained = bound.retained();

    // R1/R2 — no record at all: absence is the only admissible observation,
    // because a live root and a foreign root look exactly alike from here.
    assert_eq!(
        classify_legacy_root(None, PathEntry::Absent),
        LegacyRootDisposition::Create
    );
    for observed in [
        PathEntry::Directory(identity(1)),
        PathEntry::RegularFile,
        PathEntry::Symlink,
        PathEntry::Other,
    ] {
        assert!(
            matches!(
                classify_legacy_root(None, observed),
                LegacyRootDisposition::Refuse(_)
            ),
            "R2 {observed:?}"
        );
    }

    // R3 — interrupted before the directory syscall: nothing to reclaim, and
    // creating is not a takeover because nothing stands there.
    for record in [&bound, &unbound] {
        assert_eq!(
            classify_legacy_root(Some(record), PathEntry::Absent),
            LegacyRootDisposition::Create
        );
    }

    // R4 — the record stood while the directory appeared, and the parent lock
    // admits one creation at a time, so the directory is that creation's own.
    assert_eq!(
        classify_legacy_root(Some(&unbound), PathEntry::Directory(identity(9))),
        LegacyRootDisposition::Reclaim(identity(9))
    );
    // R5 — the exact inode the record names.
    assert_eq!(
        classify_legacy_root(Some(&bound), PathEntry::Directory(identity(1))),
        LegacyRootDisposition::Reclaim(identity(1))
    );
    // R6 — a different inode is a substitution, not a retry: same-scope ABA is
    // precisely what the recorded inode exists to catch.
    assert!(matches!(
        classify_legacy_root(Some(&bound), PathEntry::Directory(identity(2))),
        LegacyRootDisposition::Refuse(_)
    ));
    // R7 — a file or symlink was never a sandbox root of ours.
    for observed in [PathEntry::RegularFile, PathEntry::Symlink, PathEntry::Other] {
        assert!(
            matches!(
                classify_legacy_root(Some(&bound), observed),
                LegacyRootDisposition::Refuse(_)
            ),
            "R7 {observed:?}"
        );
    }

    // R8 — an explicit retention outranks every observation, including the one
    // that would otherwise be a clean reclaim. Silence is recoverable; a
    // statement that the root was kept on purpose is not.
    for observed in [
        PathEntry::Absent,
        PathEntry::Directory(identity(1)),
        PathEntry::RegularFile,
    ] {
        assert!(
            matches!(
                classify_legacy_root(Some(&retained), observed),
                LegacyRootDisposition::Refuse(_)
            ),
            "R8 {observed:?}"
        );
    }
}

/// The record survives a write/read round trip and refuses anything it did not
/// write, so a reader cannot be talked into reclaiming by a hand-made file.
#[test]
fn a_creation_record_round_trips_and_rejects_a_foreign_schema() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("scope");
    let lock = awaken_sandbox_fs::try_lock_exclusive(&base.path().join(".scope.lock")).unwrap();

    assert_eq!(read_creation_record(&lock, &root).unwrap(), None);
    let record = LegacyCreationRecord::in_flight(Some(identity(4)));
    write_creation_record(&lock, &root, &record).unwrap();
    assert_eq!(read_creation_record(&lock, &root).unwrap(), Some(record));

    retain_creation_record(&lock, &root).unwrap();
    assert!(
        read_creation_record(&lock, &root)
            .unwrap()
            .unwrap()
            .is_retained()
    );

    let path = base.path().join(".scope.awaken-legacy-creation.json");
    std::fs::write(&path, br#"{"schema":99,"root":null,"retained":false}"#).unwrap();
    let error = read_creation_record(&lock, &root).unwrap_err().to_string();
    assert!(error.contains("unsupported schema"), "{error}");

    std::fs::write(&path, b"{}").unwrap();
    assert!(read_creation_record(&lock, &root).is_err());

    std::fs::write(&path, br#"{"schema":1,"root":null,"retained":false}"#).unwrap();
    clear_creation_record(&lock, &root).unwrap();
    assert_eq!(read_creation_record(&lock, &root).unwrap(), None);
    // Idempotent: a settled creation may clear a record another path removed.
    clear_creation_record(&lock, &root).unwrap();
}
