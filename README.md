# devmil-git

Installed-Git execution for Rust applications. Git remains responsible for configuration, hooks, filters,
credential helpers, Git LFS, and repository semantics.

`Repository` owns discovery, background loading, watching, refresh, bounded
caches and recorded Git execution. Applications request typed snapshots,
history pages, commit details, diffs, blame and file history through one handle.
The crate has no UI toolkit dependencies.

Working, staged, committed and comparison diffs inspect file sizes before patch
generation. A side above `loading::diff_preview::MAX_INLINE_FILE_BYTES` (8 MiB)
returns a metadata-only `DiffFallback::TooLarge`, retaining its path, change kind,
modes and byte count. Smaller changed files in the same request still produce
inline diffs. The metadata-only fallback has zero line count and preview limit;
no line count is available because its content was not read.

`filesystem::fingerprint` provides fixed-buffer SHA-256 worktree preconditions.
`preview_fingerprint_path` uses a metadata stamp for oversized files so display
does not read their bodies. Mutation consumers must validate that stamp, capture
exact selected fingerprints before execution and recheck them before publishing
the result. A metadata stamp alone is not an exact content guard during mutation.

`HistoryIndex::ancestor_positions` walks a frozen commit ancestry through the
retained object-position lookup, without another Git read or rebuilding a
full-topology map. Missing boundary parents end the walk.

```rust,no_run
use devmil_git::{Repository, RepositoryOptions};
use devmil_git::loading::graph::{GraphHistoryOrder, GraphTraversalScope};
use devmil_git::loading::history_index::HistoryFilter;
use std::time::Duration;

let repository = Repository::new("/path/to/repository", RepositoryOptions::default())?;
let mut events = repository.subscribe();
repository.load()?; // queues discovery and the initial snapshot
let page = repository.history_page(
    GraphHistoryOrder::Topological, GraphTraversalScope::AllReferences,
    HistoryFilter::default(), 100, None,
)?;
// A UI normally polls tickets after its wakeup callback or ReadCompleted event.
if let Some(result) = page.recv_timeout(Duration::from_secs(10)) {
    let first_page = result?;
    // Pass first_page.next to history_page for a continuation of this snapshot.
    let _ = first_page;
}
let _ = events.try_recv();
repository.close();
# Ok::<(), devmil_git::model::RepositoryError>(())
```

See [Repository model](docs/repository-model.md) for lifecycle, search completeness,
pagination and cache limits. Low-level loaders, parsers and command execution are
also available for specialised consumers:

```rust,no_run
use devmil_git::activity::{ActivityEvent, ActivityId, ActivityStatus};
use devmil_git::process::{ActivityGitRunner, GitProcessCommand, ProcessArgument};

let command = GitProcessCommand::git("git")
    .current_dir("/path/to/repository")
    .args(["status", "--porcelain=v2", "-z"].map(ProcessArgument::visible))
    .output_limit(1024 * 1024);
let mut events = |event: ActivityEvent| {
    // Events are already redacted. Forward to your bounded application store.
    let _ = event;
};
let record = ActivityGitRunner::run(
    command,
    ActivityId::new(1).unwrap(),
    None,
    &mut events,
);
assert!(matches!(record.status, ActivityStatus::Succeeded { .. }));
```

Low-level callers allocate activity IDs and decide which commands to run. Every
attempt emits a creation event before spawn and a final record even on launch
failure. Keep event sinks and parser observers fast: they run synchronously on
the worker that owns the command.

Write-command builders return one `GitInvocation` (arguments, environment and standard
input). Preview it with `activity_command` and run `process_command`, so the preview and the
process cannot drift. Path arguments are redacted as `<path>` and pinned to literal pathspecs.
`RefTransaction` accepts only validated `RefId`/`ObjectId` values, so a ref name cannot inject
a command into `git update-ref --stdin -z`.

`commands::worktree::remove_arguments` rechecks live registrations under the caller's
mutation lease and returns a targeted `git worktree remove -- <path>` invocation.
Missing checkout directories can be removed without forcing deletion or pruning
other registrations. Main, current, locked, dirty and replaced checkouts remain
protected. The caller owns confirmation, execution, reconciliation and UI feedback.

## API boundaries

| Module | Responsibility |
| --- | --- |
| `model` | Long-lived repository, worker, events, queued reads, cache invalidation and mutation coordination |
| `loading` | Typed graph, metadata, diff, tree, blame, file-history and ref/status loading; history filtering and pagination |
| `cache` | Entry- and payload-bounded LRU cache with deterministic statistics |
| `process` | Commands, output transport, redaction, progress, deadlines, process trees |
| `activity` | Redacted events, records, statuses, and bounded byte buffers |
| `cancellation` | Consumer-implemented cancellation signal; `AtomicBool` works directly |
| `environment` | macOS desktop PATH discovery and explicit startup re-exec |
| `version` | Parse installed-Git versions without imposing a support floor |
| `types`, `status`, `refs` | Native repository paths, object/ref identities, status and ref models and parsers |
| `repository` | Repository discovery through a consumer-supplied recorded-command callback |
| `diff` | Raw, unified and combined diffs, numstat, index stages, selection models and partial patch construction |
| `history` | Graph records, cat-file framing, commit metadata, blame, reflog, tree and history formats |
| `commands`, `merge` | Typed branch/tag/remote/stash requests, patch export arguments, native command arguments and sequencer state; `merge::local_changes` (which local changes block an in-place revert or cherry-pick); `GitInvocation` builders for commit, stage/unstage, restore/clean/apply, worktree add, submodule, LFS, bisect, conflict index entries and `RefTransaction` (`update-ref --stdin -z`); `merge::markers` conflict-marker parsing |
| `probes` | Canonical read-only probes: `resolve`, `is_ancestor`, `unmerged_paths`, `changed_paths`, `has_changes`, `is_published`, `published_commits` |
| `index_transaction` | Staging on a private index copy and installation under `index.lock` |
| `native_bytes` | Lossless native path to Git bytes conversion (Unix raw bytes, Windows WTF-8) |
| `rebase`, `sequence_editor` | Todo grammar, edits, autosquash validation, sequencer files and editor mechanics |
| `clone`, `transfer`, `auth` | Source validation, transfer diagnostics, credential protocol and SSH parsing |
| `lfs`, `submodule` | LFS pointer/status and submodule output parsing |
| `filesystem`, `index_lock` | Capability directory identity and explicit index-lock inspection/removal |
| `git_locks` | Discovery and confirmation-bound removal of Git `*.lock` files, and lock-conflict detection in Git diagnostics |
| `status_acceleration` | Reading and opting into Git's status accelerators for large worktrees: the untracked cache and, on Windows and macOS with Git 2.37+, the built-in filesystem monitor |
| `archive` | Zip snapshots of a commit's tree under one top folder: every tracked file with its mode and links, submodules at their recorded commits, Git LFS content from the local store or `lfs smudge`, with cancellation and progress |
| `ignore` | The top-level `.gitignore` pattern that matches exactly one path, and atomic, line-ending-preserving, non-duplicating appends to the worktree's `.gitignore` |
| `diagnosis` | One table of known Git failures (host key, missing object, authentication, lock conflict, ref case collision, remote ahead, stale lease) recognised from diagnostics, with the refs they name |
| `diff::source` | Exact source identity of diff sides: full object IDs from `--full-index` headers and proof of a captured source against every displayed line |
| `syntax-highlighting` feature | `diff::syntax`: theme-independent token categories for diff sources (syntect, two-face grammars, Oniguruma), resumable checkpoints, bounds and cancellation |
| `hosting` feature | GitHub/GitLab/Forgejo models, provider requests (including web links, pull-request heads, mergeability, checks and re-runs, per-head CI status), OAuth protocol, HTTP preparation and classification, port-aware binding, preset defaults and review client |
| `integrations` | External editor, terminal, browser, diff/merge-tool templates, discovery and native launch |

Applications own UI layout and publication, confirmation, undo and recovery
policy, authentication prompts, credential
storage, and provider refresh scheduling. Parsers and builders are usable independently of
the runner. Limits belong to the caller where the format permits them.
All installed-Git commands use the shared runner.

`auth::transfer_configuration` and `auth::transfer_environment` prepare one
transfer attempt. A default attempt keeps the credential helpers of the user's
Git configuration and forbids prompts, so stored credentials answer as they do
on the command line. An attempt with an external askpass clears the helpers so
the supplied answer reaches the remote.

Enable `features = ["hosting"]` for the provider API. `hosting::providers::provider_plugin`
selects provider rules and accepts a `HostingProviderRequester` implemented by the
consumer. `hosting::review::ReviewClient` accepts an injected transport; the caller
owns authorization to submit a review. OAuth callback addresses, browser login
sessions, account persistence and credential-store namespaces belong to the caller.
Enable `features = ["syntax-highlighting"]` for `diff::syntax`. Read each diff
side's exact source through `Repository::raw_blob` (stored bytes, no filters,
textconv or external commands) or `Repository::worktree_source` (no symlink
following, never cached); both run on the background read lane. Prove the
bytes with `diff::source::verify_displayed_lines` before annotating them, and
prove worktree bytes against a fingerprint the diff read already captured.
`SyntaxAnnotator` parses whole sources from the start or a checkpoint, never a
hunk alone, and returns UTF-8 byte ranges per line in a closed category set.
Construct it on a worker of its own; when and for which lines to run it, and
how to present the categories, belong to the caller. The bundled grammars
predate some language features (Dart patterns, for example) and are not
type-aware.
`integrations::SystemIntegrationPlatform` discovers and launches external tools from
typed definitions without adding application preferences or confirmation policy.

## Output and secrets

`ProcessArgument::redacted` and `env_redacted` keep values out of published
metadata. Credential-shaped arguments, authenticated URLs, and sensitive
environment names are also recognized. A command with secrets defaults to
redacting output. `publish_sanitized_output` permits streaming with known
secret values removed across chunk boundaries, but cannot identify arbitrary
unknown secrets. Use `redact_stdout` or `redact_output` for those commands.

The `run_private` APIs expose bounded **unredacted** bytes for parsers. Never
send those buffers to a UI, activity sink, logger, or error message. Their
storage zeroizes on drop and deliberately has no `Debug` implementation.
Explicitly taking the bytes transfers responsibility for clearing them to the
caller. Truncated output is flagged; consumers must reject incomplete parser
input when completeness matters.

## Cancellation and deadlines

Use `ActivityGitRunner` for application commands. It owns the child tree and
drains both streams while checking cancellation. `GitProcessCommand::git`
defaults to a five-minute output-idle deadline, reset by incoming bytes even
after retained output fills. `GitProcessCommand::new` has no default deadline.
Total deadlines are opt-in through `SemanticTimeout` and should describe the
operation's reason for terminating. Hooks, signing, and credential helpers may
legitimately take time without producing output.

No Git command is passed through a shell. Repository-shaping inherited Git
variables are removed so an explicit working directory identifies the
repository. Explicit command environment overrides are applied afterward.
Normal host configuration, PATH, certificates, and credential helpers remain
inherited.

## Desktop PATH

Finder-launched macOS applications often miss shell-installed Git helpers such
as `git-lfs`. `environment::resolve_desktop_path` probes the user's interactive
login shell, falls back to a noninteractive login shell, and appends existing
Homebrew tool directories. Its probe has bounded output and execution time.
The probe runs in a session of its own, without a controlling terminal, for
every shell: an interactive shell started from a process with a terminal (a
launch from a command line, a CI pseudo-terminal) would otherwise be stopped
until the probe's time limit.
Other platforms return the inherited PATH.

Applications can pass the resolved PATH through command environment settings.
Applications that need the corrected PATH for every child can explicitly call
`environment::prepare_desktop_process_environment` at startup, before threads
or GUI initialization. This may replace the current process with the same
executable and arguments. A host that loads this crate as a library, such as a
Flutter runner, calls it from its own entry point before the runtime starts;
the environment of a running multi-threaded process is never changed. Library
import alone never changes global state.

## Install

```toml
[dependencies]
devmil-git = { git = "https://github.com/devmil/devmil-rust-git", rev = "<commit sha>", features = ["hosting"] }
```

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all-features
```

Tests use local processes and deterministic fixtures.
Linux and Windows require their native process-tree tests; a macOS result does
not qualify those platforms.

`branch_create_arguments` accepts `BranchTarget::Commit`, `BranchTarget::Head`, or a validated
full ref through `BranchTarget::parse` / `BranchTarget::Reference`. Remote-tracking
ref starts explicitly use `--track`, including when `branch.autoSetupMerge` is
disabled; local branch and tag refs retain Git’s normal configuration behavior.
The existing `BranchStart` API stays compatible. Callers must revalidate mutable ref targets between preview and execution.

## License

MIT. See `LICENSE`.
