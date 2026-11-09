# Repository composition and graph navigation

A brain can contain independent topic, skill and project repositories. Clone
each at its chosen path and exclude the child checkout in its parent's
`.gitignore`. Each checkout keeps its own branch, history and access permissions;
no submodule pins are required. cfetch neither creates mounts nor assumes that
each machine has separate copies of files.

`cfetch repos knowledge todo --sync` discovers repositories below those roots,
fetches their configured upstream branches, merges clean committed changes and
pushes ahead commits. Omit `--sync` for a read-only view using cached Git refs.
Use `--json` for per-repository outcomes. Dirty, detached, unborn and untracked
branches are reported without automatic commits. Existing staged changes stay
staged. Conflicting merges are previewed without writing conflict markers into
working documents; resolve them explicitly using Git. An offline repository
does not stop another repository from synchronizing. A failed synchronization
returns a nonzero status, including when some repositories succeeded.

Enable synchronization of enrolled roots in the machine configuration:

```json
{"git":{"enabled":true,"roots":["knowledge","todo"],"interval_secs":900,"debounce_ms":1000,"lock_timeout_secs":600,"timeout_secs":30}}
```

Synchronization is event-driven, not polling. A commit tool signals cfetch by
touching `git-sync.request` in the state directory after a successful push (the
file names the repository; an empty or unknown name means "all"). Local ref
changes in every enrolled checkout are watched with inotify. `interval_secs` is
the safety interval for changes no event can report — most importantly a remote
push on an NFS client, where inotify does not see another host's writes. Each
burst is debounced (`debounce_ms`) into one pass, and a pass with neither a
request nor a changed local ref is skipped, so cfetch's own merge cannot wake
it in a loop. The first pass at daemon start is immediate.

Roots relative to the brain are supported in configuration. Read operations do
not fetch implicitly. Git owns credentials and hosting; there is no provider API.
Automatic synchronization requires Git with `merge-tree --write-tree` support
for divergent branches. cfetch never resets, stashes, rebases or force pushes.

Fetch and inspect take no writer lock; they touch neither the index nor the
working tree. Only the merge (and a push that has to refetch after a rejected
race) takes it, so a read-only sync can never delay a commit. The lock has two
layers, in this order:

- `brain-commit.flock` — a kernel `flock`, shared by all worktrees. Contenders on
  one kernel queue in its wait queue with no polling, and a dead holder is
  released by the kernel. `lock_timeout_secs` bounds the wait.
- `cfetch-repository.lock` — the atomic `mkdir` directory, which remains
  required across machines: a local `flock` does not exclude an NFS client's
  lock (measured on this fleet). Its `owner` record (`host`, `pid`, `start` =
  process start time in clock ticks, `epoch`) is written right after creation
  and removed with the directory. A contender clears it only with proof that
  the owner died: a local pid gone or reused, or a remote record older than six
  hours. A lock without a parsable record is never broken.

Repository state is cached locally in `repositories.json`.

Markdown stays authoritative. The graph connects available notes across all
enrolled content, independent of repository boundaries. Missing checkouts and
ambiguous note names do not produce guessed edges.

```sh
cfetch graph --focus architecture
cfetch graph-path architecture deployment --depth 6 --json
```

Paths follow explicit wikilinks and backlinks, with original link directions
retained in JSON. The shortest route is bounded to at most 32 hops. The same
operations are available over MCP as `cfetch_graph` and `cfetch_graph_path`.
