# dsopt

**Find and reclaim disk space from build artifacts — `node_modules` and Rust `target/` directories.**

`dsopt` walks a filesystem once with a parallel native scanner, shows you what those directories
actually cost, and removes the ones you pick. Built as a single Rust binary on
[ratatui](https://github.com/ratatui/ratatui) + crossterm — no Python, no virtualenv, no runtime
dependencies.

Website: **https://handyutils.github.io/dsopt** · Crate: **https://crates.io/crates/dsopt**

## Install

```bash
cargo install dsopt
```

Prebuilt archives for macOS (Apple Silicon and Intel), Linux x64/ARM64, and Windows x64 are attached
to every [release](https://github.com/handyutils/dsopt/releases/latest).

### Update

```bash
dsopt --update      # check crates.io and reinstall when a newer release exists
dsopt --update --force   # reinstall even when already current
```

## Use

```bash
dsopt                      # open the TUI and scan the whole filesystem
dsopt ~/code ~/work        # scan only the places that grow
dsopt --threads 8          # 1-8 scanner threads (default 4)
dsopt --list               # print a table instead of opening the TUI
dsopt --list --json        # one JSON object per line, for scripts
```

### Flags

| Flag | Meaning |
| --- | --- |
| `ROOTS...` | Roots to scan. Defaults to `/`. |
| `--threads <N>` | Scanner threads, 1–8. Default 4. |
| `--list` | Print candidates instead of opening the TUI. |
| `--json` | With `--list`, emit one JSON object per line. |
| `--update` | Install the newest published release. |
| `--force` | With `--update`, reinstall even when already current. |

### Keyboard shortcuts

| Key | Action |
| --- | --- |
| `Space` | select / unselect the highlighted directory |
| `A` / `N` | select all / select none |
| `D` | remove the selected directories |
| `R` | rescan the filesystem |
| `T` | scanner thread settings |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | move through candidates |
| `?` | help overlay |
| `Q` | quit |

## What it detects

| Detector | Directory name | Typed as |
| --- | --- | --- |
| JavaScript dependencies | `node_modules` | `node_modules` |
| Rust build output | `target` | `rust_target` |

Nested detectors collapse into their outermost parent, so a single removal reclaims the whole subtree
rather than double-counting a `node_modules` buried inside another project's dependencies. Candidates
are deduplicated by `(device, inode)`, which stops macOS volume aliases from being counted twice.

## Safety

Removal is permanent — there is no Trash and no undo, and the TUI says so before doing anything.
A directory is deleted only when all of the following hold:

- it resolved successfully and is a real directory;
- it sits strictly inside the scan root, and is not the root itself;
- it is not `/`, `/System`, `/Users`, or `/Applications`;
- it is not under a virtual filesystem or macOS volume alias — `/dev`, `/proc`, `/sys`,
  `/System/Volumes/Data`, `/System/Volumes/VM`, `/System/Volumes/Preboot`, `/System/Volumes/Update`,
  `/private/var/vm`, `/private/var/db/dyld`.

The same checks run again immediately before each delete, not only when the scan is collected.

## How it works

- **Scan** — [`parallel-disk-usage`](https://crates.io/crates/parallel-disk-usage) builds a size tree
  over the root with a rayon thread pool; a second pass collects and sorts candidates. Live counters
  are shared through atomics and streamed to the status line while the walk runs.
- **Cache** — the last completed scan is written to `~/.cache/dsopt/last_scan.json` (honouring
  `XDG_CACHE_HOME`), so reopening `dsopt` is instant. Press `R` for a fresh walk.
- **Remove** — deletions run on a background thread and report progress per directory, so the UI
  never blocks.

## Development

```bash
cargo build            # debug binary at target/debug/dsopt
cargo test             # unit + integration tests
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

The library (`src/lib.rs`, `src/app.rs`, `src/update.rs`) holds all logic; `dsopt.rs` is the thin CLI
entry point. CI runs format, tests, clippy, and `cargo package` on every push.

## License

MIT © HandyUtils