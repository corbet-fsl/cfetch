use super::*;

#[test]
#[cfg(unix)]
fn timed_out_git_helpers_cannot_keep_mutating_after_the_lock_is_released() {
    let root = tempfile::tempdir().unwrap();
    let error = git_output(
        root.path(),
        &[
            "-c",
            "alias.slow=!sleep 1; printf orphan > orphan-marker",
            "slow",
        ],
        Duration::from_millis(100),
    )
    .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!root.path().join("orphan-marker").exists());
}
const TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    _dir: tempfile::TempDir,
    remote: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        git(
            dir.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
            TIMEOUT,
        )
        .unwrap();
        git(
            dir.path(),
            &[
                "clone",
                "-c",
                "core.autocrlf=false",
                remote.to_str().unwrap(),
                a.to_str().unwrap(),
            ],
            TIMEOUT,
        )
        .unwrap();
        {
            let path = &a;
            git(path, &["config", "user.name", "Test Writer"], TIMEOUT).unwrap();
            git(path, &["config", "core.autocrlf", "false"], TIMEOUT).unwrap();
            git(
                path,
                &["config", "user.email", "test@example.invalid"],
                TIMEOUT,
            )
            .unwrap();
        }
        std::fs::write(a.join("note.md"), "original\n").unwrap();
        commit(&a, "note.md");
        git(&a, &["push", "-u", "origin", "main"], TIMEOUT).unwrap();
        git(
            dir.path(),
            &[
                "clone",
                "-c",
                "core.autocrlf=false",
                remote.to_str().unwrap(),
                b.to_str().unwrap(),
            ],
            TIMEOUT,
        )
        .unwrap();
        git(&b, &["config", "user.name", "Test Writer"], TIMEOUT).unwrap();
        git(&b, &["config", "core.autocrlf", "false"], TIMEOUT).unwrap();
        git(
            &b,
            &["config", "user.email", "test@example.invalid"],
            TIMEOUT,
        )
        .unwrap();
        Self {
            _dir: dir,
            remote,
            a,
            b,
        }
    }
}

fn commit(root: &Path, path: &str) {
    git(root, &["add", "--", path], TIMEOUT).unwrap();
    git(root, &["commit", "-qm", "record change"], TIMEOUT).unwrap();
}

#[test]
fn independent_edits_merge_and_publish_without_a_special_transport() {
    let f = Fixture::new();
    std::fs::write(f.a.join("first.md"), "first\n").unwrap();
    commit(&f.a, "first.md");
    std::fs::write(f.b.join("second.md"), "second\n").unwrap();
    commit(&f.b, "second.md");
    synchronize(&f.a, TIMEOUT, TIMEOUT).unwrap();
    assert_eq!(synchronize(&f.b, TIMEOUT, TIMEOUT).unwrap().state, "synchronized");
    synchronize(&f.a, TIMEOUT, TIMEOUT).unwrap();
    assert_eq!(
        std::fs::read_to_string(f.a.join("second.md")).unwrap(),
        "second\n"
    );
    assert!(f.b.join("first.md").is_file());
    assert_eq!(
        inspect(&f.a, TIMEOUT).unwrap().head,
        inspect(&f.b, TIMEOUT).unwrap().head
    );
}

#[test]
fn conflicting_edits_leave_worktree_and_index_unchanged() {
    let f = Fixture::new();
    std::fs::write(f.a.join("note.md"), "alpha\n").unwrap();
    commit(&f.a, "note.md");
    std::fs::write(f.b.join("note.md"), "beta\n").unwrap();
    commit(&f.b, "note.md");
    synchronize(&f.a, TIMEOUT, TIMEOUT).unwrap();
    let before = inspect(&f.b, TIMEOUT).unwrap().head;
    assert_eq!(synchronize(&f.b, TIMEOUT, TIMEOUT).unwrap().state, "conflict");
    assert_eq!(
        std::fs::read_to_string(f.b.join("note.md")).unwrap(),
        "beta\n"
    );
    assert_eq!(inspect(&f.b, TIMEOUT).unwrap().head, before);
    assert!(!dirty(&f.b, TIMEOUT).unwrap());
    assert!(!in_progress(&f.b, TIMEOUT).unwrap());
}

#[test]
fn dirty_and_staged_work_are_never_committed_or_replaced() {
    let f = Fixture::new();
    std::fs::write(f.a.join("note.md"), "unpublished\n").unwrap();
    git(&f.a, &["add", "--", "note.md"], TIMEOUT).unwrap();
    std::fs::write(f.a.join("note.md"), "still editing\n").unwrap();
    assert_eq!(synchronize(&f.a, TIMEOUT, TIMEOUT).unwrap().state, "dirty");
    assert_eq!(
        git(&f.a, &["show", ":note.md"], TIMEOUT).unwrap(),
        "unpublished\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.a.join("note.md")).unwrap(),
        "still editing\n"
    );
}

#[test]
fn nested_ignored_repositories_are_discovered_and_locked_independently() {
    let f = Fixture::new();
    let child = f.a.join("skills/shared");
    std::fs::create_dir_all(&child).unwrap();
    git(&child, &["init", "--initial-branch=main"], TIMEOUT).unwrap();
    std::fs::write(f.a.join(".gitignore"), "/skills/shared/\n").unwrap();
    let repos = discover(std::slice::from_ref(&f.a)).unwrap();
    assert_eq!(repos.len(), 2);
    let _parent_lock = lock(&f.a, TIMEOUT).unwrap();
    assert!(lock(&f.a, TIMEOUT).is_err());
    assert!(lock(&child, TIMEOUT).is_ok());
}

#[test]
fn unavailable_remote_does_not_block_another_repository() {
    let f = Fixture::new();
    git(
        &f.a,
        &["remote", "set-url", "origin", "/missing/example.git"],
        TIMEOUT,
    )
    .unwrap();
    let statuses = run(&[f.a.clone(), f.b.clone()], true, TIMEOUT, TIMEOUT).unwrap();
    assert_eq!(statuses[0].state, "error");
    assert_eq!(statuses[1].state, "synchronized");
    assert!(f.remote.is_dir());
}

/// The read-only fetch must not wait for the writer lock: a current checkout
/// syncs while another writer holds it, because fetch touches neither the
/// index nor the working tree. This is the behaviour that ended the give-ups.
#[test]
fn a_current_fetch_does_not_take_the_writer_lock() {
    let f = Fixture::new();
    let _held = lock(&f.a, TIMEOUT).unwrap();
    assert_eq!(
        synchronize(&f.a, TIMEOUT, TIMEOUT).unwrap().state,
        "synchronized"
    );
}

/// The merge still takes the writer lock: a behind checkout must wait, and
/// its working files stay exactly as they were when the lock is unavailable.
#[test]
fn a_merge_waits_for_the_writer_lock() {
    let f = Fixture::new();
    std::fs::write(f.b.join("second.md"), "second\n").unwrap();
    commit(&f.b, "second.md");
    git(&f.b, &["push", "origin", "main"], TIMEOUT).unwrap();
    let _held = lock(&f.a, TIMEOUT).unwrap();
    let error = synchronize(&f.a, TIMEOUT, Duration::from_millis(300)).unwrap_err();
    assert!(error.to_string().contains("repository busy"), "{error:#}");
    assert!(!f.a.join("second.md").exists());
    // The read-only fetch did run (the upstream ref moved); only the merge
    // was refused by the held writer lock.
    assert_eq!(inspect(&f.a, TIMEOUT).unwrap().state, "behind");
}

#[test]
fn a_pass_targets_only_what_woke_it() {
    let f = Fixture::new();
    let repos = vec![
        Enrolled::new(f.a.clone(), TIMEOUT).unwrap(),
        Enrolled::new(f.b.clone(), TIMEOUT).unwrap(),
    ];
    assert!(select_targets(&repos, &[], None, false).is_empty());
    assert_eq!(
        select_targets(&repos, &[], None, true),
        vec![f.a.clone(), f.b.clone()]
    );
    assert_eq!(
        select_targets(&repos, &[1], None, false),
        vec![f.b.clone()]
    );
    assert_eq!(
        select_targets(&repos, &[], Some(f.a.to_str().unwrap()), false),
        vec![f.a.clone()]
    );
    // A signal that names no enrolled checkout must not be dropped: sync all.
    assert_eq!(
        select_targets(&repos, &[], Some("/nowhere"), false).len(),
        2
    );
}

/// Local ref changes are what the worker watches; a remote-tracking update
/// (our own fetch) must NOT look like a local change or the worker would sync
/// itself in a loop.
#[test]
fn the_local_digest_changes_on_commit_but_not_on_fetch() {
    let f = Fixture::new();
    let repo = Enrolled::new(f.a.clone(), TIMEOUT).unwrap();
    let before = repo.digest();
    std::fs::write(f.a.join("third.md"), "third\n").unwrap();
    commit(&f.a, "third.md");
    let after_commit = repo.digest();
    assert_ne!(before, after_commit);
    // Advance the remote from the other clone and fetch into a: refs/remotes
    // moves, refs/heads does not.
    std::fs::write(f.b.join("fourth.md"), "fourth\n").unwrap();
    commit(&f.b, "fourth.md");
    git(&f.b, &["push", "origin", "main"], TIMEOUT).unwrap();
    git(&f.a, &["fetch", "-q", "origin"], TIMEOUT).unwrap();
    assert_eq!(after_commit, repo.digest());
}

#[test]
#[cfg(target_os = "linux")]
fn the_lock_carries_an_owner_record_and_removes_it_with_the_lock() {
    let root = tempfile::tempdir().unwrap();
    git_output(root.path(), &["init", "-q"], TIMEOUT).unwrap();
    let lock_path = root.path().join(".git").join("cfetch-repository.lock");
    {
        let held = lock(root.path(), TIMEOUT).unwrap();
        let owner = std::fs::read_to_string(held.path.join("owner")).unwrap();
        let field = |name: &str| {
            owner
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{name}=")))
                .unwrap()
                .to_owned()
        };
        assert_eq!(field("pid"), std::process::id().to_string());
        assert!(!field("host").is_empty());
        assert!(field("start").parse::<u64>().is_ok());
        assert!(field("epoch").parse::<u64>().unwrap() > 1_700_000_000);
        assert!(
            lock(root.path(), TIMEOUT).is_err(),
            "a second holder is refused"
        );
        assert!(!held.path.join("owner.tmp").exists());
    }
    assert!(
        !lock_path.exists(),
        "the lock and its record are gone after release"
    );
}
