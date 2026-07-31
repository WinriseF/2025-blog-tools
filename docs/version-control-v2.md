# WinriseF Version Control Bridge V2

Version Control V2 is a local-first, read-mostly protocol exposed by the portable WinriseF Toolbox Agent. It is independent from Transfer Bridge V3. Git never contacts a remote; SVN history contacts its repository only after the explicit native confirmation described below.

## Launch and transport

- Launch URI: `winrisef://launch?returnUrl=...&nonce=...&feature=version-control`.
- Callback fragment marker: `winrisef-version-control=1`.
- Endpoint: pinned WebTransport on `127.0.0.1` at `/winrisef/version-control/v2`.
- The exact browser Origin and one-time 128-bit launch token are required.
- The control stream uses big-endian `u32 length + JSON`, with a hard 64KiB JSON limit.
- A second launch while any Agent feature owns the per-user mutex returns `error=agent_busy` through the callback.

## Control messages

The first message is `hello` with protocol version `2` and the one-time launch token. Supported commands are `select-repository`, `open-repository-candidate`, `connect-history`, `close-repository`, `get-repository-overview`, `refresh-repository`, `get-history-page`, `open-diff`, `get-diff-files-page`, `open-file-preview`, `prepare-export`, `confirm-export`, and `cancel-export`.

Every request after hello has a numeric `requestId`. Repository, diff, file, revision, and export references are created or validated by the Agent. The browser cannot submit a filesystem path. History and file pages are reduced until their serialized response fits the control-frame budget.

`prepare-export` sends file selection as `{ mode: include|exclude, ranges: [[startId, endId], ...] }` with sorted, non-overlapping inclusive ranges. The browser chooses the shorter include/exclude representation, and the Agent expands and validates it against the authorized Diff. This keeps the normal all-files and directory-selection cases within the control-frame budget without accepting arbitrary paths.

Diff sessions keep one bounded metadata record per changed path and the Agent retains at most three sessions. The Agent locates `svn.exe` once per process; `svn info --xml` both validates that executable and identifies the selected working copy. Opening the normal BASE-to-working-copy view reuses `svn status --xml --verbose` as its authoritative file list instead of running a duplicate `svn diff --summarize`; historical and arbitrary-revision comparisons keep the summary command for authoritative A/M/D/property metadata. Each SVN Diff runs `svn diff --git` once and splits the raw bytes into per-file Patch text and text-hunk counters. Malformed byte sequences are replaced only in this review representation, so one non-UTF-8 file cannot fail the whole workspace. The Patch cache is bounded to three revision ranges and 32MiB. Unversioned working-copy text files are read locally within the preview limit because SVN omits them from `svn diff`. `refresh-repository` invalidates worktree-sensitive status, summary, Patch, and source caches. Commit-to-commit comparisons do not read or attach current worktree status. Closing the control session invalidates repository state and cancels any active export.

## Preview stream

`open-file-preview` includes `mode: patch|full`, acknowledges on the control stream, then opens an Agent-to-browser unidirectional stream:

```text
u32 metadataLength
metadata JSON { requestId, originalBytes, modifiedBytes }
original UTF-8 bytes
modified UTF-8 bytes
```

Patch mode is the default SVN review path. It streams the cached per-file Patch in `original`, leaves `modified` empty, and starts no `svn cat` command. The browser renders only hunk context and changed lines with real old/new line numbers; omitted spans are placeholders. Patch decoding is lossy by design so malformed bytes remain inspectable without aborting unrelated files.

Full mode is limited to 2MiB per side. Binary, invalid UTF-8, and oversized files do not produce full source bodies. Symlinks expose the link value and are not followed. LFS pointer files remain pointers.

SVN loads the two historical source sides concurrently on a preview cache miss. Complete source bodies use a repository-scoped LRU cache bounded to 64 entries and 32MiB, keyed by revision and relative path. Immutable revision entries survive a worktree refresh; working-copy entries are removed. Diff sessions keep their smaller rendered-preview cache so reopening the same file does not rebuild strings or start another command.

## Export exception

Git inspection is read-only. Export is an explicit user-owned filesystem write selected through the native save dialog. `prepare-export` returns only an opaque target ID and whether the target is inside the repository. Inside targets require `allowInsideRepository=true`. The Agent writes a same-directory temporary file, syncs it, atomically replaces the chosen target, and cleans the temporary file on cancellation or failure. SVN advertises export as unavailable until a backend-specific patch/export contract is added. Export completion does not expose the absolute path to the browser.

Native GitPatch export rebuilds the libgit2 Diff once, indexes deltas by path, and emits selected patches one file at a time. A native patch does not require loading both complete source bodies; conflict/fallback patches load them within the normal export limit.

## Limits and exclusions

- Preview: 2MiB per side.
- Export: 32MiB per side, one file at a time.
- Binary files are metadata-only and cannot be selected for export.
- No checkout, switch, fetch, pull, push, stage, commit, restore, reset, or stash mutation.
- Git and SVN are separate backend implementations. SVN calls the system `svn.exe` directly with `--non-interactive --no-auth-cache`; it never reads `.svn/wc.db`, invokes a shell, recurses externals, or enables `svn+ssh://` tunnels.
- SVN history requires the explicit `connect-history` command and a native confirmation before the Agent contacts the repository URL. SVN history is rendered as a single parent lane and never as a fabricated Git DAG.
- The vendored libgit2 build excludes SSH, HTTPS, and OpenSSL features because the Git backend never contacts a remote.
