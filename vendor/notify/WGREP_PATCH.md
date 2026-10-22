# wgrep patch to notify 8.2.0

This directory is a copy of the crates.io `notify` 8.2.0 source with wgrep-specific changes, applied through `[patch.crates-io]` in `crates/wisp-grep/Cargo.toml`.

- In `src/inotify.rs` (`AWatches::add_single_watch`), the Linux inotify watch mask no longer includes `WatchMask::OPEN`. Opens never change file contents, and reads of the watched tree would otherwise wake the daemon with events that it discards. Writes, truncates, creates, deletes, renames, attribute changes, and close-after-write still produce events.
- In `src/windows.rs`, zero-byte successful `ReadDirectoryChangesW` completions report an error instead of parsing an empty buffer. Unexpected completion and submission errors also reach the event handler as errors. Root disappearance reports an error before unwatching; normal operation-aborted cancellation remains silent.
- In `src/fsevent.rs`, event paths no longer require UTF-8. The callback builds each `PathBuf` from the raw `CStr` bytes with `OsStr::from_bytes`, so a non-UTF-8 file name cannot panic the watcher thread. Watch and unwatch paths return a notify error when they are not valid UTF-8 instead of using `to_str().unwrap()`. Unknown FSEvents flags are truncated with `StreamFlags::from_bits_truncate` instead of a panic, so a future macOS flag does not stop the watcher.
- In `src/fsevent.rs`, a macOS-only test module (`wgrep_device_purge_tests`) checks that stopping one watcher does not purge the queued events of a second watcher on the same volume, and that the second watcher still receives new events.

See `docs/WGREP_P0_WINDOWS_WATCHER_HANDOFF.md` for the Windows change, focused verification, and known limits.
