# Repository model

`Repository` is the normal entry point for an application. Construct it from a
path and `RepositoryOptions`, subscribe to its events, then call `load()`.
Construction performs no Git or filesystem discovery on the caller's thread.

The repository owns its worker, native watcher, refresh coalescing, recorded Git
execution, immutable snapshots, read caches and request queue. The application
does not poll files or assemble Git queries. It consumes typed snapshots and
requests additional pages or details through the same repository handle.

## Loading and events

`load()` queues discovery and initial status/ref loading and returns immediately.
Repeated calls share the same worker. `snapshot()` returns the latest immutable
`Arc<RepositorySnapshot>`; check its phase and error fields before displaying it
as loaded. Subscribe before loading to receive the initial transition.
The initial load reads references and status concurrently. A `Loading` snapshot
carrying references is published as soon as they are read, so consumers can show
branches and history before a large worktree scan finishes; `Ready` follows with
status. Later refreshes publish status and references together.
`RepositoryOptions::wakeup` can schedule a UI event-loop callback; it runs on the
worker and must return promptly. Drain events and tickets on the UI side.

`WatchOverflow` is delivered before the recovery snapshot. The model schedules
the rescan itself; consumers may display a warning without issuing another
refresh. An overflow noticed together with an explicit refresh raises no event:
that refresh already rescans what the watcher lost. This covers the events a
checkout produces while a mutation lease parks the worker. Watcher failures are reported in snapshots and retried with bounded
backoff. Unchanged watch plans reuse their existing registrations.

Read methods return `ReadTicket<T>`. `try_recv()` is nonblocking;
`recv_timeout()` is useful for command-line clients and tests. Successful results
are `Arc<T>`, and duplicate pending requests share work. Dropping a ticket drops
that subscriber; work continues while another subscriber remains. Dropping the
last subscriber cancels and reaps its Git process. `refresh()` queues
a refresh; `reload()` also returns a ticket for the published snapshot. That
ticket resolves when the requested refresh publishes, ahead of reads queued
before it, so reporting a mutation does not wait for unrelated reads. `close()`
cancels outstanding commands, joins the worker and releases history sessions;
the same handle cannot reopen. Construct another `Repository` to reopen.

## History pages and searches

`history(order, scope)` returns a frozen topology with lazy metadata.
`history_prefix(order, scope, limit)` has a separate cache key and bounds the
initial traversal. `commit_metadata(ids)` performs bounded preflight and content
batches. Commit detail/files/diff, tree, blame, file-history and reflog reads use
the same repository queue.

`statistics()` reuses loaded status and reference counts and runs one additional
commit-count query. `comparison()` shares cached commit/range diffs;
`search_diff_content()` and `bisect_state()` provide bounded history-tool reads.
`commit_tools()` reads recent messages and the configured commit template.
Templates can live outside the watched repository, so commit-tools results are
shared while in flight but are not retained after completion.

`working_diff()` returns a streaming ticket with a bounded first-file slot and a
completion result. First-file delivery wakes the consumer before the Git
process finishes. Duplicate requests share the process and completed results
are cached until invalidation. Dropping the final subscriber cancels the read.

`predict_conflicts()` simulates merges or rebases in temporary object storage;
it does not change repository refs, the index or the worktree. `commit_evidence()`
and `branch_stack()` expose Git evidence for fixup and stacked-branch workflows.
Applications decide how to rank suggestions and authorize restacking.
`content()` reads bounded bytes from a commit, HEAD, the index or the worktree.
Git filters remain active for object-backed content. Image decoding and syntax
highlighting belong to consumers of those bytes.

`FrozenTopology` retains an `Arc<LoadedTopology>` and provides shared row windows,
continuation lookup, topology comparison and bounded condition projection without
copying the complete graph. A consumer's page resolver can use this source and
add application generations and selection/layout tokens; the repository retains the topology allocation. Worktree-only edits preserve
both full and prefix topology caches. Full refreshes invalidate them.

Graph value-cache implementation and byte accounting live in `cache::graph`.
The consumer supplies presentation revision keys and invalidation triggers and
keeps its own lane, collapse, selection and search-highlighting behavior. The
filtered `history_page` API below is available to clients that want matching rows
as pages, rather than a layout with highlighted search matches.

`history_page(order, scope, filter, limit, token)` owns its history index inside
the repository. The page contains ordered commit IDs, the total match count and
an optional continuation token. Pass the same filter and token for the next page;
continuations perform no Git commands. Tokens reject other snapshots or filters,
metadata revisions, evicted sessions and full repository refreshes. Worktree-only
refreshes preserve history sessions. Call with no token to restart after rejection.

`HistoryFilter` supports ref ancestry and hash, message, author and path search
scopes. Text scopes combine with OR and matching uses ASCII case folding over
Git bytes. Message/author search hydrates every commit in bounded batches before
matching, so unopened rows are searchable. Missing, oversized or malformed
metadata produces an error instead of an incomplete result. Path searches use
the snapshot's frozen tips and cache matches by text; exceeding the result limit
is an error. `search_history_paths(roots, query, maximum_matches)` also exposes
cached exact-path or display-substring searches for consumers with frozen roots.

## Cache accounting

`cache_bytes` is an accounted-payload budget, not a process RSS limit: allocator,
map-node and thread overhead are not measured. Half is reserved for queued read
results and half for retained history sessions, including their metadata, path
matches and bounded filter caches. Large read results may be returned without
being retained. A history index or complete search that cannot fit its share
returns a capacity error. Increase the budget or use bounded low-level loaders
for that workload. External `Arc` results retained by callers are outside the
repository's ownership and can keep their allocations alive after eviction.
`cache_entries` independently bounds each cache's entries. Per-read weights
include known payload lengths/capacities and conservative record allowances;
they do not represent an exact allocator measurement.

## External mutations

Use a mutation lease when an application performs a Git write outside the model:

```rust,no_run
# use devmil_git::{Repository, RepositoryOptions};
# let repository = Repository::new("/path/to/repository", RepositoryOptions::default())?;
let lease = repository.begin_mutation()?;
// Run the authorized Git mutation through the installed-Git command adapter.
// Retain the lease until that operation and its children have finished.
drop(lease); // invalidates cached reads and queues a full refresh
# Ok::<(), devmil_git::model::RepositoryError>(())
```

Acquisition waits for active reads and refreshes; the worker stays suspended
while the lease is held. Do not wait for a model read while holding the lease,
or acquire a lease from a worker callback. Nested leases on one thread are
rejected. `begin_mutation_with_cancellation` also supports cancellation while
waiting. The lease does not authorize the write or manage an external process.

## Ownership

- Discovery resolves linked-worktree and common-directory identity once.
- Watchers register before the initial snapshot so writes during loading cause
  another refresh. Missing native watching is reported and can use bounded polling.
  Recursive backends (FSEvents, ReadDirectoryChanges) start watching before the
  first scan and receive ignored-directory exclusions in place once discovered, so
  no rescan is needed. Explicit-watch backends register after discovering
  exclusions and rescan the interval between the first scan and registration.
- Worktree-only changes invalidate status and working-tree diffs. Ref, HEAD,
  config, `.gitattributes`, sequencer and overflow events invalidate the corresponding repository
  reads. Full refreshes also discard object caches because replace refs and
  grafts can change the interpretation of an object ID.
- Duplicate pending reads share work. Pages and batched metadata use typed keys
  and bounded caches; output truncation is an error, never an apparently complete page.
- Snapshot revisions identify snapshot publications; generic read tickets are
  not revision-tagged. History continuations have their own snapshot identity.
  Closing cancels outstanding Git children and stops watcher delivery.
- Subscribers have bounded event retention. A slow subscriber receives an explicit
  resynchronization event and reads the latest snapshot.

The application retains UI layout, selection, dialogs, credential persistence and
authorization decisions. It may supply activity recording, credentials, wakeup
notifications and tuning options. It does not supply the repository's refresh loop.

Hosting and external-tool APIs remain separate entry points. Low-level parsers
and loaders are available for specialised consumers and deterministic tests.

## Integration checks

Tests must exercise opening, initial load, actual filesystem changes, burst
coalescing, cache hits, paging, duplicate reads, watcher overflow, cancellation,
slow subscribers, linked worktrees and close/reopen. Git invocation counts must
verify cache and batch behaviour.

## Large worktree opening

With `refresh_read_policy: RefreshReadPolicy::ConcurrentImmutable`, one bounded refresh lane performs status/ref scans while the read worker can serve
immutable history and object reads. Mutable reads wait for the scan to publish.
Both lanes participate in the mutation lease and shutdown joins both. Explicit
refresh requests, including mutation reconciliation, block queued reads until the
new snapshot is published. The option defaults to `AfterSnapshot` to preserve existing consumers' first-snapshot ordering. Watch-driven scans retain interactive immutable reads;
cache invalidation still distinguishes full and worktree-only refreshes.

`RefreshReadPolicy::ConcurrentScoped` also admits path-scoped working-tree reads
during a scan: a working diff with a selected file asks Git for that path
directly, so a slow status in a large worktree does not hold it back. A
whole-worktree diff still waits. A finished scan discards cached scoped results,
and explicit refresh requests still order every read behind them.

Before a watcher-triggered scan starts, the worker answers up to eight queued
interactive reads. Events arriving during one scan therefore cannot chain scans
indefinitely ahead of reads that waited for the previous one.

Status scans use a session-private copy of supported full Git indices, capped at
32 MiB. Git refreshes this copy's stat cache without holding the user's index lock
during the scan. When Git had to rewrite a freshly copied index (for example after
a copy or checkout changed every file's stat data), the session installs that copy
once, as Git's optional locks would: it takes `index.lock` exclusively, skips when
another process holds it, verifies the user's index is unchanged and renames the
copy into place. Later sessions then avoid re-reading every file.
Source bytes are checked before reuse and after execution; external staging or
replacement discards the copy. Its initial mtime is preserved for Git's racy-clean
checks. Split, sparse, version-4, unborn, oversized and unreadable indices use the
original read-only path. Private files are removed when the session closes.

Ignored-directory exclusions are discovered on their own thread at opening and
after each explicit refresh, because Git walks the worktree to list them. The
worker keeps serving reads meanwhile. A failed discovery keeps the exclusions
already in use, or watches without any when none were found yet. It does not
report automatic updates as unavailable when the watcher itself registered
successfully. Actual watcher failures retain their existing
recovery and bounded polling behavior.
