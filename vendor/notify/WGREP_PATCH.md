# wgrep patch to notify 8.2.0

This directory is a copy of the crates.io `notify` 8.2.0 source with wgrep-specific changes, applied through `[patch.crates-io]` in `crates/wisp-grep/Cargo.toml`.

- In `src/inotify.rs` (`AWatches::add_single_watch`), the Linux inotify watch mask no longer includes `WatchMask::OPEN`. Opens never change file contents, and reads of the watched tree would otherwise wake the daemon with events that it discards. Writes, truncates, creates, deletes, renames, attribute changes, and close-after-write still produce events.
- In `src/windows.rs`, zero-byte successful `ReadDirectoryChangesW` completions report an error instead of parsing an empty buffer. Unexpected completion and submission errors also reach the event handler as errors. Root disappearance reports an error before unwatching; normal operation-aborted cancellation remains silent.

See `docs/WGREP_P0_WINDOWS_WATCHER_HANDOFF.md` for the Windows change, focused verification, and known limits.
