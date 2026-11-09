//! Ordinary Git repositories are the sharing boundary. Each checkout is
//! independent; an offline or conflicted repository never blocks its peers.
//! Filesystem placement is outside this module's contract.

use anyhow::{Context as _, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    /// Enroll existing checkouts explicitly. A directory enrolls repositories
    /// below it as well, including ignored independently owned child repos.
    pub roots: Vec<PathBuf>,
    pub enabled: bool,
    /// Safety interval, not the primary cadence. A post-push signal from
    /// brain-commit or a local ref change syncs immediately; this only bounds
    /// how long a change no event can see (a remote push while this host
    /// watches nothing) may wait.
    pub interval_secs: u64,
    pub timeout_secs: u64,
    /// Coalesce a burst of signals this long before one sync pass, so one
    /// commit does not turn into a fetch per event.
    pub debounce_ms: u64,
    /// Bound for waiting on the brain writer lock. Fetch never takes it; a
    /// merge takes it only for local Git work.
    pub lock_timeout_secs: u64,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            enabled: false,
            interval_secs: 900,
            timeout_secs: 30,
            debounce_ms: 1000,
            lock_timeout_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryStatus {
    pub path: PathBuf,
    pub state: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub detail: Option<String>,
    /// True when this pass actually contacted the remote (`git fetch`). Dirty,
    /// detached, unborn and untracked checkouts are inspected but never
    /// fetched, so this is the number "fetches/day" must count.
    #[serde(default)]
    pub fetched: bool,
}

impl RepositoryStatus {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            state: "unknown".into(),
            branch: None,
            head: None,
            ahead: 0,
            behind: 0,
            detail: None,
            fetched: false,
        }
    }
}

/// Never follow directory symlinks or enter Git internals. Filesystem mount
/// points behave like any other directory. Unreadable roots are errors,
/// rather than silently turning a missing checkout into an empty brain.
pub fn discover(roots: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut seen = BTreeSet::new();
    let mut repositories = BTreeSet::new();
    let mut pending: Vec<PathBuf> = roots.to_vec();
    while let Some(path) = pending.pop() {
        let path = path
            .canonicalize()
            .with_context(|| format!("open repository root {}", path.display()))?;
        if !seen.insert(path.clone()) {
            continue;
        }
        ensure!(
            seen.len() <= 100_000,
            "repository discovery exceeds 100000 directories; enroll narrower roots"
        );
        if path.join(".git").exists() {
            repositories.insert(path.clone());
        }
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && !entry.file_name().to_string_lossy().starts_with('.')
                && !matches!(
                    entry.file_name().to_str(),
                    Some("node_modules" | "target" | "models")
                )
            {
                pending.push(entry.path());
            }
        }
    }
    Ok(repositories.into_iter().collect())
}

/// The brain's writer lock, in the SAME two layers brain-commit uses:
///
/// - `brain-commit.flock` (kernel flock) orders contenders on one kernel —
///   srv and archlxc share it — with no poll and no staleness concept.
/// - `cfetch-repository.lock` (atomic mkdir) is the cross-boundary mutex for
///   the NFS client, whose flock lives in a different lock domain. It is
///   historical but still required; see the brain-commit README for the
///   measurement that settled this.
///
/// Fetch/inspect are deliberately NOT covered: they never touch the index or
/// the working tree, so a read-only sync cannot block a commit.
struct RepositoryLock {
    path: PathBuf,
    _flock: crate::lockfile::Lock,
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        for name in ["owner", "owner.tmp", "info"] {
            let _ = std::fs::remove_file(self.path.join(name));
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// Start time of this process in clock ticks since boot (field 22 of /proc/self/stat); empty off Linux.
/// Together with the pid it identifies the process even after the pid is reused.
fn process_start() -> String {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            let after = stat.rsplit_once(") ")?.1.to_owned();
            after.split_whitespace().nth(19).map(str::to_owned)
        })
        .unwrap_or_default()
}

/// The owner record other cooperating writers (brain-commit) use to tell a live lock from a dead one: the
/// same `key=value` lines, written right after the directory is created. A holder that crashed leaves
/// a record whose pid is gone or reused, which lets them clear the lock; a lock without a record is never broken.
fn write_owner(path: &Path) {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = format!(
        "host={}\npid={}\nstart={}\nepoch={epoch}\ntime={epoch}\n",
        host.trim(),
        std::process::id(),
        process_start(),
    );
    let temporary = path.join("owner.tmp");
    if std::fs::write(&temporary, record).is_ok() {
        let _ = std::fs::rename(&temporary, path.join("owner"));
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn process_start_of(pid: u64) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(") ")?.1;
    after.split_whitespace().nth(19).map(str::to_owned)
}

/// A legacy mkdir lock is stale only with PROOF, exactly like brain-commit:
/// a local owner whose pid is gone or reused, or a remote record older than
/// six hours. A lock without a parsable record is never broken.
fn legacy_lock_is_stale(path: &Path) -> Option<String> {
    let snapshot = std::fs::read_to_string(path.join("owner")).ok()?;
    let field = |name: &str| {
        snapshot
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map(str::to_owned)
    };
    let host = field("host")?;
    let pid: u64 = field("pid")?.parse().ok()?;
    let start = field("start").unwrap_or_default();
    let epoch: u64 = field("epoch")?.parse().ok()?;
    let reason = if host == hostname() {
        // Proving a local owner dead needs /proc. On a platform without it we
        // cannot prove anything, so the lock is never broken (the same rule as
        // a missing owner record).
        #[cfg(target_os = "linux")]
        {
            match process_start_of(pid) {
                None => format!("owner pid {pid} on this host is gone"),
                Some(now) if !start.is_empty() && now != start => {
                    format!("pid {pid} was reused (start {start} -> {now})")
                }
                Some(_) => return None,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pid, &start);
            return None;
        }
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        const STALE_REMOTE_SECS: u64 = 6 * 3600;
        if now.saturating_sub(epoch) <= STALE_REMOTE_SECS {
            return None;
        }
        format!(
            "owner on {host} is {} s old (bound {STALE_REMOTE_SECS} s)",
            now.saturating_sub(epoch)
        )
    };
    // Move it away atomically so exactly one breaker wins, then re-read to be
    // sure the record judged is still the record present.
    if std::fs::read_to_string(path.join("owner")).ok()? != snapshot {
        return None;
    }
    let gone = path.with_extension(format!("stale.{}", std::process::id()));
    std::fs::rename(path, &gone).ok()?;
    let _ = std::fs::remove_dir_all(&gone);
    Some(reason)
}

/// The writer lock. Callers take it only around a merge (never a fetch or a
/// push), so the mkdir layer changes hands in milliseconds.
fn lock(root: &Path, timeout: Duration) -> anyhow::Result<RepositoryLock> {
    let common = git(root, &["rev-parse", "--git-common-dir"], timeout)?;
    let git_common = root.join(common.trim());
    let flock_path = git_common.join("brain-commit.flock");
    let _flock = crate::lockfile::acquire(
        &flock_path,
        timeout.as_millis().min(u64::MAX as u128) as u64,
        0,
    )
    .with_context(|| format!("repository busy or lock inaccessible: {}", flock_path.display()))?;
    let path = git_common.join("cfetch-repository.lock");
    let started = std::time::Instant::now();
    loop {
        match std::fs::create_dir(&path) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(reason) = legacy_lock_is_stale(&path) {
                    eprintln!(
                        "cfetch Git: broke stale lock {}: {reason}",
                        path.display()
                    );
                    continue;
                }
                if started.elapsed() >= timeout {
                    anyhow::bail!(
                        "repository busy: the writer lock {} stayed held for {}s",
                        path.display(),
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("repository lock inaccessible: {}", path.display()));
            }
        }
    }
    write_owner(&path);
    Ok(RepositoryLock { path, _flock })
}

fn git_output(root: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<(bool, String)> {
    use std::io::{Read as _, Seek as _};
    let mut output = tempfile::tempfile()?;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        // Remote diagnostics can contain credential-bearing URLs. State and
        // operation are reported, never raw transport stderr.
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().context("start Git")?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            #[cfg(unix)]
            {
                let _ = rustix::process::kill_process_group(
                    rustix::process::Pid::from_child(&child),
                    rustix::process::Signal::KILL,
                );
            }
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "Git {} timed out; inspect repository state before retrying",
                args[0]
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    output.rewind()?;
    let mut bytes = Vec::new();
    output.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "Git response exceeds 4 MiB");
    Ok((
        status.success(),
        String::from_utf8(bytes).context("Git returned non-UTF-8 output")?,
    ))
}

fn git(root: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    let (ok, output) = git_output(root, args, timeout)?;
    ensure!(
        ok,
        "Git {} failed; repository changes were not discarded",
        args[0]
    );
    Ok(output)
}

fn optional(root: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<Option<String>> {
    let (ok, value) = git_output(root, args, timeout)?;
    Ok(ok.then(|| value.trim().to_string()))
}

fn in_progress(root: &Path, timeout: Duration) -> anyhow::Result<bool> {
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
    ] {
        let path = git(root, &["rev-parse", "--git-path", name], timeout)?;
        if root.join(path.trim()).exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn dirty(root: &Path, timeout: Duration) -> anyhow::Result<bool> {
    Ok(!git(
        root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
        timeout,
    )?
    .is_empty())
}

fn counts(root: &Path, remote: &str, timeout: Duration) -> anyhow::Result<(usize, usize)> {
    let value = git(
        root,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{remote}"),
        ],
        timeout,
    )?;
    let mut parts = value.split_whitespace();
    Ok((
        parts.next().context("missing ahead count")?.parse()?,
        parts.next().context("missing behind count")?.parse()?,
    ))
}

fn inspect(root: &Path, timeout: Duration) -> anyhow::Result<RepositoryStatus> {
    let mut result = RepositoryStatus::new(root);
    result.branch = optional(
        root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        timeout,
    )?;
    result.head = optional(root, &["rev-parse", "--verify", "HEAD^{commit}"], timeout)?;
    result.state = if in_progress(root, timeout)? {
        "conflict"
    } else if result.head.is_none() {
        "unborn"
    } else if result.branch.is_none() {
        "detached"
    } else if dirty(root, timeout)? {
        "dirty"
    } else if let Some(upstream) = optional(
        root,
        &["rev-parse", "--verify", "@{upstream}^{commit}"],
        timeout,
    )? {
        (result.ahead, result.behind) = counts(root, &upstream, timeout)?;
        match (result.ahead, result.behind) {
            (0, 0) => "current",
            (_, 0) => "ahead",
            (0, _) => "behind",
            _ => "diverged",
        }
    } else {
        "untracked"
    }
    .into();
    Ok(result)
}

/// Sync only already committed work on an attached branch with an upstream.
/// User/editor changes are never swept into an automatic commit. Clean text
/// merges are previewed in Git's object store so conflicts do not write
/// conflict markers into live Markdown. No reset, stash, rebase or force push.
///
/// The fetch and the push are deliberately lock-free: they touch neither the
/// index nor the working tree, so a read-only sync can never make a commit
/// wait. Only the merge takes the writer lock, for the seconds it works
/// locally. A rejected push (another host won the race) refetches and merges
/// again, bounded.
fn synchronize(
    root: &Path,
    timeout: Duration,
    lock_timeout: Duration,
) -> anyhow::Result<RepositoryStatus> {
    let mut result = inspect(root, timeout)?;
    if matches!(
        result.state.as_str(),
        "conflict" | "dirty" | "unborn" | "detached" | "untracked"
    ) {
        return Ok(result);
    }
    let branch = result.branch.clone().context("missing branch")?;
    let remote = git(
        root,
        &["config", "--get", &format!("branch.{branch}.remote")],
        timeout,
    )?;
    let target = git(
        root,
        &["config", "--get", &format!("branch.{branch}.merge")],
        timeout,
    )?;
    let (remote, target) = (remote.trim().to_owned(), target.trim().to_owned());
    ensure!(
        !remote.is_empty() && target.starts_with("refs/heads/") && !target.contains('\n'),
        "upstream must identify one branch"
    );
    let mut last_push_error: Option<anyhow::Error> = None;
    for _attempt in 0..3 {
        // Read-only network step, no writer lock.
        let fetched = match git(
            root,
            &[
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                &remote,
                &target,
            ],
            timeout,
        ) {
            Ok(_) => {
                let value = git(
                    root,
                    &["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
                    timeout,
                )?;
                value.trim().to_owned()
            }
            // Offline: nothing can be decided against a remote that did not
            // answer. The status is an error, not a silent "current".
            Err(error) => return Err(error),
        };
        // Another editor may have started work during the network wait.
        if dirty(root, timeout)? || in_progress(root, timeout)? {
            return inspect(root, timeout);
        }
        let observed = inspect(root, timeout)?;
        if observed.branch.as_deref() != Some(branch.as_str()) {
            return Ok(RepositoryStatus {
                state: "changed".into(),
                detail: Some("checkout changed during fetch; retrying later".into()),
                ..observed
            });
        }
        result = observed;
        result.fetched = true;
        if !fetched.is_empty() {
            (result.ahead, result.behind) = counts(root, &fetched, timeout)?;
            if result.behind > 0 {
                // Writer lock: only a merge mutates the shared tree.
                let _lock = lock(root, lock_timeout)?;
                // Re-check under the lock: a local brain-commit may have
                // integrated the same upstream while we were on the network.
                let current = inspect(root, timeout)?;
                if current.branch != result.branch {
                    return Ok(RepositoryStatus {
                        state: "changed".into(),
                        detail: Some("checkout changed before merge; retrying later".into()),
                        ..current
                    });
                }
                let head = current.head.clone().context("missing HEAD")?;
                let already = optional(
                    root,
                    &["merge-base", "--is-ancestor", &fetched, &head],
                    timeout,
                )?;
                if already.is_none() {
                    let (ahead_now, _) = counts(root, &fetched, timeout)?;
                    if ahead_now > 0 {
                        let (clean, _) = git_output(
                            root,
                            &["merge-tree", "--write-tree", "HEAD", &fetched],
                            timeout,
                        )?;
                        if !clean {
                            result.state = "conflict".into();
                            result.detail = Some(
                                "upstream cannot be merged automatically; working files are unchanged"
                                    .into(),
                            );
                            return Ok(result);
                        }
                    }
                    if let Err(error) = git(
                        root,
                        &["merge", "--no-edit", "--no-stat", "--no-autostash", &fetched],
                        timeout,
                    ) {
                        // A dirty file of another session is in the way: report
                        // the observed state and leave the tree exactly as it
                        // was. Only a real merge that started is aborted.
                        if in_progress(root, timeout)? {
                            let _ = git(root, &["merge", "--abort"], timeout);
                        }
                        let observed = inspect(root, timeout)?;
                        return Ok(RepositoryStatus {
                            state: observed.state,
                            detail: Some(format!(
                                "upstream merge refused; working files are unchanged ({error:#})"
                            )),
                            ..observed
                        });
                    }
                }
            }
            if counts(root, &fetched, timeout)?.0 > 0 {
                let refspec = format!("refs/heads/{branch}:{target}");
                match git(
                    root,
                    &[
                        "push",
                        "--porcelain",
                        "--no-follow-tags",
                        "--",
                        &remote,
                        &refspec,
                    ],
                    timeout,
                ) {
                    Ok(_) => {
                        result = inspect(root, timeout)?;
                        result.fetched = true;
                        result.state = "synchronized".into();
                        result.ahead = 0;
                        result.behind = 0;
                        return Ok(result);
                    }
                    Err(error) => {
                        // Another host advanced the branch between our fetch
                        // and push. Refetch and integrate once more.
                        last_push_error = Some(error);
                        continue;
                    }
                }
            }
        }
        // Nothing of ours is ahead of the fetched tip.
        result = inspect(root, timeout)?;
        result.fetched = true;
        result.state = "synchronized".into();
        result.ahead = 0;
        result.behind = 0;
        return Ok(result);
    }
    Err(last_push_error.unwrap_or_else(|| anyhow::anyhow!("push failed after 3 attempts")))
}

pub fn run(
    roots: &[PathBuf],
    sync: bool,
    timeout: Duration,
    lock_timeout: Duration,
) -> anyhow::Result<Vec<RepositoryStatus>> {
    ensure!(!timeout.is_zero(), "Git timeout must be positive");
    let repositories = discover(roots)?;
    Ok(repositories
        .iter()
        .map(|root| {
            let result = if sync {
                synchronize(root, timeout, lock_timeout)
            } else {
                inspect(root, timeout)
            };
            result.unwrap_or_else(|error| {
                let mut status = RepositoryStatus::new(root);
                status.state = "error".into();
                status.detail = Some(format!("{error:#}"));
                status
            })
        })
        .collect())
}

/// File in the state directory that brain-commit touches after a successful
/// push. It is NEVER removed by either side: a touch (mtime advance) is the
/// signal and a consumer compares mtimes, so there is no create/remove race
/// and a signal delivered while the daemon is down still triggers one pass.
pub const SYNC_REQUEST_FILE: &str = "git-sync.request";

fn request_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// One enrolled checkout with its resolved Git directory. Reading HEAD and the
/// branch ref from disk is what tells the worker a LOCAL commit or branch
/// switch happened without spawning `git` on every event.
struct Enrolled {
    root: PathBuf,
    git_dir: PathBuf,
    /// Branch refs live here even for a linked worktree (its own git-dir
    /// carries HEAD, the shared one carries refs/).
    common_dir: PathBuf,
}

impl Enrolled {
    fn new(root: PathBuf, timeout: Duration) -> Option<Self> {
        let dir = optional(&root, &["rev-parse", "--absolute-git-dir"], timeout).ok()??;
        let common = optional(&root, &["rev-parse", "--git-common-dir"], timeout).ok()??;
        let common = root.join(common.trim());
        Some(Self {
            root,
            git_dir: PathBuf::from(dir.trim()),
            common_dir: common,
        })
    }

    /// The value that changes on a local commit, merge, or checkout: the ref
    /// HEAD names, plus its object id. A packed ref (after gc) is covered by
    /// the packed-refs mtime/length.
    fn digest(&self) -> String {
        let head = std::fs::read_to_string(self.git_dir.join("HEAD")).unwrap_or_default();
        let name = head
            .trim()
            .strip_prefix("ref: ")
            .map(str::to_owned)
            .unwrap_or_else(|| "detached".into());
        let mut value = format!("{name}={}", head.trim());
        match std::fs::read(self.common_dir.join(&name)) {
            Ok(bytes) => value.push_str(&String::from_utf8_lossy(&bytes)),
            Err(_) => {
                if let Ok(meta) = std::fs::metadata(self.common_dir.join("packed-refs")) {
                    value.push_str(&format!(
                        ";packed={:?}/{}",
                        meta.modified().ok(),
                        meta.len()
                    ));
                }
            }
        }
        value
    }
}

fn enrolled(roots: &[PathBuf], timeout: Duration) -> Vec<Enrolled> {
    discover(roots)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|root| Enrolled::new(root, timeout))
        .collect()
}

fn digests(repos: &[Enrolled]) -> Vec<String> {
    repos.iter().map(Enrolled::digest).collect()
}

fn write_status(status: &[RepositoryStatus]) {
    if let Ok(body) = serde_json::to_string_pretty(status)
        && let Err(error) = crate::fsutil::atomic_write(
            &crate::paths::state_dir().join("repositories.json"),
            &body,
        )
    {
        eprintln!("cfetch Git: write repositories.json: {error:#}");
    }
}

/// Replace (or append) the status entries for `changed` in the persisted
/// status file, so a targeted sync does not erase the other repositories.
fn publish_status(status: Vec<RepositoryStatus>) {
    let path = crate::paths::state_dir().join("repositories.json");
    let mut merged: Vec<RepositoryStatus> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    for entry in status {
        match merged.iter_mut().find(|old| old.path == entry.path) {
            Some(old) => *old = entry,
            None => merged.push(entry),
        }
    }
    merged.sort_by(|a, b| a.path.cmp(&b.path));
    write_status(&merged);
}

/// Event-driven synchronization. Three wake sources:
///   1. brain-commit's post-push touch of [`SYNC_REQUEST_FILE`] (a commit is
///      worth an immediate fetch of that repository);
///   2. a local ref change in any enrolled checkout (HEAD/branch, watched with
///      inotify) — commits made outside brain-commit still sync;
///   3. the safety interval, which bounds what no event can see: a remote
///      push on an NFS client (inotify does not report other hosts' writes)
///      or a watch that could not be registered.
///
/// A burst is debounced into one pass, and a pass that is neither requested
/// nor a real ref change is skipped — that is what stops cfetch's own merge
/// from waking itself in a loop. Fetch and inspect never take the writer lock.
pub fn worker(cfg: crate::config::Config, stopping: impl Fn() -> bool) {
    let timeout = Duration::from_secs(cfg.git.timeout_secs);
    let lock_timeout = Duration::from_secs(cfg.git.lock_timeout_secs);
    let safety = Duration::from_secs(cfg.git.interval_secs);
    let debounce = Duration::from_millis(cfg.git.debounce_ms);
    let roots: Vec<_> = cfg.git.roots.iter().map(|path| cfg.resolve(path)).collect();
    let state_dir = crate::paths::state_dir();
    let _ = std::fs::create_dir_all(&state_dir);
    let request_path = state_dir.join(SYNC_REQUEST_FILE);
    // Persist the signal file so "never created yet" and "touched at boot"
    // are distinguishable without a create/remove race.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&request_path);
    let mut watch_errors = 0usize;
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
    let filter_request = request_path.clone();
    let filter_state = state_dir.clone();
    let mut watcher = match notify::recommended_watcher(
        move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else { return };
            // The state directory also holds repositories.json, which every
            // pass rewrites: only the request file there is a signal, or the
            // worker would wake itself after every pass.
            let interesting = event
                .paths
                .iter()
                .any(|path| path == &filter_request || !path.starts_with(&filter_state));
            if interesting {
                let _ = wake_tx.send(());
            }
        },
    ) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            eprintln!("cfetch Git: file watching unavailable, safety interval only: {error}");
            None
        }
    };
    let mut watched: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut repos = enrolled(&roots, timeout);
    watch_errors += watch_paths(&state_dir, &repos, &mut watcher, &mut watched);
    if watch_errors > 0 {
        eprintln!(
            "cfetch Git: {watch_errors} watch(es) unavailable; the {}s safety interval still covers them",
            safety.as_secs()
        );
    }
    let mut last_digests = digests(&repos);
    let mut last_request = request_mtime(&request_path);
    // The first pass is immediate; later ones wait out the safety interval.
    let mut next_safety = Instant::now();
    loop {
        if stopping() {
            return;
        }
        let wait = next_safety
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(200));
        let woke = match wake_rx.recv_timeout(wait) {
            Ok(()) => true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if woke {
            // Event bursts settle for one debounce window; a barrier is not
            // relevant here (no query waits on a Git sync).
            std::thread::sleep(debounce);
            while wake_rx.try_recv().is_ok() {}
        }
        let requested_at = request_mtime(&request_path);
        let requested = requested_at != last_request;
        last_request = requested_at;
        let now_digests = digests(&repos);
        let changed: Vec<usize> = now_digests
            .iter()
            .enumerate()
            .filter(|(index, digest)| last_digests.get(*index) != Some(*digest))
            .map(|(index, _)| index)
            .collect();
        let safety_due = Instant::now() >= next_safety;
        if !requested && changed.is_empty() && !safety_due {
            continue;
        }
        // A post-push request may name one repository; the others keep their
        // safety cadence. Local ref changes select exactly the repositories
        // whose digest moved; the safety tick refreshes everything.
        let named = if requested {
            std::fs::read_to_string(&request_path)
                .ok()
                .and_then(|text| text.lines().next().map(str::trim).map(str::to_owned))
                .filter(|text| !text.is_empty())
        } else {
            None
        };
        let targets = select_targets(&repos, &changed, named.as_deref(), safety_due);
        if targets.is_empty() {
            last_digests = now_digests;
            if safety_due {
                next_safety = Instant::now() + safety;
            }
            continue;
        }
        let mut reasons = Vec::new();
        if safety_due {
            reasons.push("safety");
        }
        if !changed.is_empty() {
            reasons.push("local-ref");
        }
        if requested {
            reasons.push("request");
        }
        match run(&targets, true, timeout, lock_timeout) {
            Ok(status) => {
                // One line per pass: each entry is one fetch, so the count is
                // the operator's answer to "how much network is this doing?".
                let fetches = status.iter().filter(|entry| entry.fetched).count();
                eprintln!(
                    "cfetch Git: {fetches} fetch(es) across {} repositor{} ({})",
                    status.len(),
                    if status.len() == 1 { "y" } else { "ies" },
                    reasons.join("+")
                );
                publish_status(status);
            }
            Err(error) => eprintln!("cfetch Git: {error:#}"),
        }
        // Keep the digests taken BEFORE the pass: a commit that arrived while
        // the pass ran must still look changed on the next wake-up, or that
        // event would be swallowed.
        last_digests = now_digests;
        if safety_due {
            next_safety = Instant::now() + safety;
            // Repositories may have appeared (and refs may have been packed):
            // converge the watch set on the same cadence.
            let fresh = enrolled(&roots, timeout);
            if fresh.len() != repos.len()
                || fresh
                    .iter()
                    .zip(&repos)
                    .any(|(a, b)| a.root != b.root || a.git_dir != b.git_dir)
            {
                repos = fresh;
                last_digests = digests(&repos);
            }
            let _ = watch_paths(&state_dir, &repos, &mut watcher, &mut watched);
        }
    }
}

/// Which repositories one pass must sync:
/// - the safety tick refreshes everything (it is also the only source that can
///   see a remote push on an NFS client);
/// - local ref changes select exactly the checkouts whose digest moved;
/// - a post-push request naming one enrolled checkout syncs only that one, and
///   an empty or unknown target falls back to everything (a signal must never
///   be silently dropped);
/// - nothing requested and nothing changed means no pass at all.
fn select_targets(
    repos: &[Enrolled],
    changed: &[usize],
    named: Option<&str>,
    safety_due: bool,
) -> Vec<PathBuf> {
    let mut targets: Vec<PathBuf> = if safety_due {
        repos.iter().map(|r| r.root.clone()).collect()
    } else {
        changed
            .iter()
            .filter_map(|index| repos.get(*index).map(|repo| repo.root.clone()))
            .collect()
    };
    if let Some(named) = named {
        let named = Path::new(named);
        match repos.iter().find(|repo| repo.root == named) {
            Some(repo) => targets.push(repo.root.clone()),
            None => targets.extend(repos.iter().map(|repo| repo.root.clone())),
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

/// Registers watches for the request file's directory and every checkout's
/// HEAD/ref tree. Returns the number of paths that could not be watched; the
/// safety interval remains the floor for those.
fn watch_paths(
    state_dir: &Path,
    repos: &[Enrolled],
    watcher: &mut Option<notify::RecommendedWatcher>,
    watched: &mut std::collections::HashSet<PathBuf>,
) -> usize {
    use notify::Watcher as _;
    let Some(watcher) = watcher.as_mut() else {
        return 0;
    };
    let mut failures = 0usize;
    let mut add = |path: PathBuf, mode: notify::RecursiveMode, failures: &mut usize| {
        if !watched.insert(path.clone()) {
            return;
        }
        if let Err(error) = watcher.watch(&path, mode) {
            // Do not remember a failed path: the next safety tick retries it.
            watched.remove(&path);
            *failures += 1;
            eprintln!("cfetch Git: cannot watch {}: {error}", path.display());
        }
    };
    add(
        state_dir.to_path_buf(),
        notify::RecursiveMode::NonRecursive,
        &mut failures,
    );
    for repo in repos {
        add(
            repo.git_dir.clone(),
            notify::RecursiveMode::NonRecursive,
            &mut failures,
        );
        add(
            repo.common_dir.join("refs/heads"),
            notify::RecursiveMode::Recursive,
            &mut failures,
        );
    }
    failures
}

#[cfg(test)]
mod tests;
