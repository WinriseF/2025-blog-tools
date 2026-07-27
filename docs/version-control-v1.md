# WinriseF Version Control Bridge V1

Version Control V1 is a local-only, read-mostly protocol exposed by the portable WinriseF Toolbox Agent. It is independent from Transfer Bridge V3.

## Launch and transport

- Launch URI: `winrisef://launch?returnUrl=...&nonce=...&feature=version-control`.
- Callback fragment marker: `winrisef-version-control=1`.
- Endpoint: pinned WebTransport on `127.0.0.1` at `/winrisef/version-control/v1`.
- The exact browser Origin and one-time 128-bit launch token are required.
- The control stream uses big-endian `u32 length + JSON`, with a hard 64KiB JSON limit.
- A second launch while any Agent feature owns the per-user mutex returns `error=agent_busy` through the callback.

## Control messages

The first message is `hello` with protocol version `1` and the one-time launch token. Supported commands are `select-repository`, `close-repository`, `get-repository-overview`, `refresh-repository`, `get-history-page`, `open-diff`, `get-diff-files-page`, `open-file-preview`, `prepare-export`, `confirm-export`, and `cancel-export`.

Every request after hello has a numeric `requestId`. Repository, diff, file, revision, and export references are created or validated by the Agent. The browser cannot submit a filesystem path. History and file pages are reduced until their serialized response fits the control-frame budget.

`prepare-export` sends file selection as `{ mode: include|exclude, ranges: [[startId, endId], ...] }` with sorted, non-overlapping inclusive ranges. The browser chooses the shorter include/exclude representation, and the Agent expands and validates it against the authorized Diff. This keeps the normal all-files and directory-selection cases within the control-frame budget without accepting arbitrary paths.

Diff sessions keep one bounded metadata record per changed path and the Agent retains at most three sessions. Commit-to-commit comparisons do not read or attach current worktree status. Closing the control session invalidates repository state and cancels any active export.

## Preview stream

`open-file-preview` acknowledges on the control stream, then opens an Agent-to-browser unidirectional stream:

```text
u32 metadataLength
metadata JSON { requestId, originalBytes, modifiedBytes }
original UTF-8 bytes
modified UTF-8 bytes
```

Each side is limited to 2MiB. Binary, invalid UTF-8, and oversized files do not produce source bodies. Symlinks expose the link value and are not followed. LFS pointer files remain pointers.

## Export exception

Git inspection is read-only. Export is an explicit user-owned filesystem write selected through the native save dialog. `prepare-export` returns only an opaque target ID and whether the target is inside the repository. Inside targets require `allowInsideRepository=true`. The Agent writes a same-directory temporary file, syncs it, atomically replaces the chosen target, and cleans the temporary file on cancellation or failure. Export completion does not expose the absolute path to the browser.

Native GitPatch export rebuilds the libgit2 Diff once, indexes deltas by path, and emits selected patches one file at a time. A native patch does not require loading both complete source bodies; conflict/fallback patches load them within the normal export limit.

## Limits and exclusions

- Preview: 2MiB per side.
- Export: 32MiB per side, one file at a time.
- Binary files are metadata-only and cannot be selected for export.
- No checkout, switch, fetch, pull, push, stage, commit, restore, reset, or stash mutation.
- Git V1 does not contain SVN abstractions or dependencies.
- The vendored libgit2 build excludes SSH, HTTPS, and OpenSSL features because V1 never contacts a remote.
