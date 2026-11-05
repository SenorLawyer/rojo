# memofs Changelog

## Unreleased Changes

* Added `StdBackend::new_without_watch` and `Vfs::new_default_without_watch` to avoid allocating an OS watcher for one-shot operations.
* Failed watch registrations can be retried, and removing a directory clears descendant watch registrations.
* Mapped directories can be watched again after deletion and recreation, using a surviving nonrecursive parent watch.
* Unwatching an unregistered path no longer restarts the macOS event stream and loses unrelated changes.
* Watcher errors preserve their underlying OS error details.
* A removal and recreation that `notify` merges into one write is reported as `VfsEvent::Remove` followed by `VfsEvent::Create`, so watches on the deleted directory are replaced.
* The same applies when `notify` reports a replaced directory only as a creation, and to replacements whose merged event arrives up to 60 seconds after the removal. A directory whose recursive watch fails keeps its parent watch.
* Watch roots lost because an ancestor was removed are registered again when they reappear, and reported with `VfsEvent::Create`.
* Added `VfsEvent::Rescan`, emitted when the watcher may have dropped events. Watcher errors are logged instead of ignored.
* `read_dir` registers its watch before listing, so entries created during the listing are reported.
* Failing to watch a parent directory no longer fails the read that requested the watch.
* Failing to watch a path for any reason other than it missing, such as reaching the inotify watch limit or lacking permission, is logged once and no longer fails the read. The watch is retried after a rescan.
* Watching a missing path reports `io::ErrorKind::NotFound` on every platform.
* After a rescan, existing watch roots are registered again, so on Linux a root replaced while events were dropped, and directories created meanwhile, are watched again.
* On Linux, removing a watch root nested inside another watch root no longer removes the outer root's watches below it.
* Parent watches used to notice recreated roots are only placed on the parent of a path, or inside directories that were watched before. The filesystem root and a parent reached through `..`, such as the project's parent for `../shared`, are not watched unless they were watched before, so a missing path cannot put a watch on the home directory through `..`.
* Where no watch can report the creation of a path passed to `Vfs::watch_missing`, the path is checked for every 2 seconds and reported with `VfsEvent::Create` once it exists. This is always the case on macOS, where FSEvents cannot watch a path before it exists, and elsewhere for paths such as `../shared` whose parent may not be watched.
* A watch root that cannot be registered again after a rescan, for example because the inotify watch limit was reached, is no longer kept as watched while directories created in it go unwatched. It is degraded and logged like any other watch failure, or treated as lost if it disappeared.
* Paths below a path that could not be watched are read without watches, and without a warning each. Directories that could not be watched for the creation of a path are not retried until a rescan.
* On Windows, where `notify` never reports a rescan, paths that could not be watched are retried every 30 seconds, reported with `VfsEvent::Write`.
* A recursive watch that fails because a directory below it disappeared mid-walk is retried, at first immediately, and a symlink whose target is missing reports `io::ErrorKind::NotFound` instead of counting as unwatchable.
* Paths are no longer considered inside a directory through a `..` component, so a missing `../shared` path cannot put a parent watch above the directories watched before.
* Added `Vfs::watch_missing`, which reports the creation of a path that does not exist yet, even when its parent is missing too.
* Watch roots recovered during a rescan are no longer also reported with `VfsEvent::Create`, since the rescan resyncs everything.
* On macOS, watches survive the removal of their path, since FSEvents reports the path again once it is recreated, and no parent watches are added. Parent watches made FSEvents report every change in the parent's subtree, and each watch change restarted its event stream.

# 0.4.0 (2026-07-01)
* Added `Vfs::canonicalize`. [#1201]
* **Breaking:** `StdBackend::new` and `Vfs::new_default` now return `io::Result`, so a failure to create the filesystem watcher is reported as an error instead of panicking. The `Default` implementation for `StdBackend` has been removed as a result. [#1267]

[#1267]: https://github.com/rojo-rbx/rojo/pull/1267

## 0.3.1 (2025-11-27)
* Added `Vfs::exists`. [#1169]
* Added `create_dir` and `create_dir_all` to allow creating directories. [#937]

[#1169]: https://github.com/rojo-rbx/rojo/pull/1169
[#937]: https://github.com/rojo-rbx/rojo/pull/937

## 0.3.0 (2024-03-15)
* Changed `StdBackend` file watching component to use minimal recursive watches. [#830]
* Added `Vfs::read_to_string` and `Vfs::read_to_string_lf_normalized` [#854]

[#830]: https://github.com/rojo-rbx/rojo/pull/830
[#854]: https://github.com/rojo-rbx/rojo/pull/854

## 0.2.0 (2021-08-23)
* Updated to `crossbeam-channel` 0.5.1.

## 0.1.3 (2020-11-19)
* Added `set_watch_enabled` to `Vfs` and `VfsLock` to allow turning off file watching.

## 0.1.2 (2020-03-29)
* `VfsSnapshot` now implements Serde's `Serialize` and `Deserialize` traits.

## 0.1.1 (2020-03-18)
* Improved error messages using the [fs-err](https://crates.io/crates/fs-err) crate.

## 0.1.0 (2020-03-10)
* Initial release
