# axtail

**axtail** (advanced tail) is a fast, read-only terminal log viewer for sysadmins.
It helps you find the root cause in log files over SSH: plain files, gzip, zip
and Docker container logs, all in one coloured text interface.

```
 axtail 0.1.0 - /var/log/app
┌ text ──────────────────────────────────── 260.4 MB / 521.8 MB (49%) ┐
│2026-11-22T06:19:59Z ERROR [worker-15] request id=2999999 took 609ms │
│2026-11-22 FATAL database connection pool exhausted                  │
│2026-11-22T06:20:01Z INFO [worker-1] request id=3000001 took 611ms   │
└─────────────────────────────────────────────────────────────────────┘
 LIVE  streaming in real time
^F Search     ^P Older Match   ^N Newer Match   Enter Live/Pause  ^L File List
Up/Dn Scroll  Lt/Rt Pan        Home/End Top/Bottom  ^G About      ^X Exit
```

## Features

- **Live by default.** The viewer follows the log in real time (`LIVE` in the status line).
- **Fast search.** `Ctrl+F` pauses streaming and searches the whole log, plain text or regex.
- **Multi-GB files.** Files are never loaded whole. axtail seeks from the end and reads
  only a small window around what is on screen.
- **gzip and zip.** Archives are stream-decompressed in memory, never extracted to disk.
- **Docker.** Reads container output through `docker logs`, and can list files inside a container.
- **File list.** Point it at a directory, a wildcard or an archive and pick a log with the arrow keys.
- **Read-only.** Logs are only ever opened for reading.
- **Cross-platform.** Linux/Unix terminals and Windows (PowerShell, Windows Terminal).

## Usage

```
axtail                                  Show usage examples
axtail <FILTER>...                      Browse local logs
axtail --docker <CONTAINER> [FILTER]... Browse a container's logs
```

`FILTER` is a file, a directory or a wildcard such as `*.log`, `*.zip`, `*.txt`
or `api-*.log`. Directories, `.zip` and `.gz` files are scanned and every log
inside is listed. Add `--grep` for advanced (regex) search.

```sh
axtail /var/log                          # every log in a directory
axtail '*.log'                           # logs in the current directory
axtail 'api-*.log' --grep 'ERROR|timeout'  # regex active, match count per file
axtail archive.zip                       # logs inside a zip
axtail app.log.gz                        # gzip, stream-decompressed
axtail --docker myapp                    # live container logs
axtail --docker myapp '/var/log/*.log'   # also list files inside the container
axtail --docker myapp --grep 'panic'
```

| Option | Meaning |
| --- | --- |
| `--docker <CONTAINER>` | Read the container's logs with `docker logs`. With a filter, also list matching files inside the container. |
| `--grep <PATTERN>` | Start with a regex search active and show the number of matching lines per file in the list. |
| `-i`, `--ignore-case` | Make `--grep` case-insensitive. |
| `--tail <LINES>` | Lines kept in memory for Docker and compressed logs (default 10000, max 200000). |
| `-h`, `--help` / `-V`, `--version` | Help / version. |

## Keys

All available keys are always shown at the bottom of the screen.

**File list**

| Key | Action |
| --- | --- |
| `Up` / `Down` | Select a log |
| `Enter` | Open the selected log |
| `Left` / `Right` | Change sort order (name, newest, largest, matches) |
| `Ctrl+F` | Filter the list by name |
| `Ctrl+G` | About |
| `Ctrl+X` | Exit |

**Log viewer**

| Key | Action |
| --- | --- |
| `Up` / `Down`, `PageUp` / `PageDown` | Scroll (pauses live) |
| `Left` / `Right` | Pan long lines |
| `Home` / `End` | Jump to the start / back to the end (live) |
| `Enter` | Toggle live / paused |
| `Ctrl+F` | Search (pauses live). `Tab` in the prompt switches text / case-sensitive / regex |
| `Ctrl+P` / `Ctrl+N` | Older / newer match |
| `Esc` | Cancel a running search, clear the search, or go back to the list |
| `Ctrl+L` | Back to the file list |
| `Ctrl+G` | About |
| `Ctrl+X` | Exit |

## Build and run

Requires a [Rust toolchain](https://rustup.rs) (1.87 or newer).

```sh
./build.sh          # Linux/Unix: builds ./axtail in the repository root
./run.sh /var/log   # builds if needed, then runs
```

```powershell
.\build.ps1         # Windows: builds .\axtail.exe in the repository root
.\run.ps1 C:\logs
```

**Windows without Visual Studio.** Only [rustup](https://rustup.rs) is required.
`build.ps1` uses the MSVC toolchain when the Visual Studio C++ build tools are installed
and otherwise falls back to the GNU toolchain (`stable-x86_64-pc-windows-gnu`), which
rustup downloads together with its own linker. No MinGW installation is needed, and all
dependencies are pure Rust. To force a toolchain:

```powershell
.\build.ps1 -Toolchain gnu    # never needs Visual Studio
.\build.ps1 -Toolchain msvc
```

If rustup asks about Visual Studio during installation, choose to continue without it.

Run the tests with `cargo test`.

## How it stays light on the server

- **Bounded memory.** The viewer holds one window of lines (about 1 MB for plain files,
  at most `--tail` lines and 8 MB for streams). Lines longer than 8 KB are split, so a
  single giant line cannot exhaust memory.
- **Plain files** are read with seeks. Opening a 50 GB file reads about 1 MB from its end.
  Live mode only checks the file size and reads the newly appended bytes. Truncation and
  rotation are detected.
- **gzip / zip / Docker** cannot be seeked, so they are streamed through a fixed-size
  ring buffer. Jumping far back re-streams from the start, which costs CPU time but no
  extra memory or disk.
- **Search** scans in 1 MB chunks on a background thread, shows progress and can be
  cancelled with `Esc`. The interface stays responsive.
- **Live Docker output** flows through a bounded channel, so a very chatty container is
  slowed down by backpressure instead of filling memory.
- **Idle cost.** The screen is only redrawn when something changes.

## Docker notes

- `axtail --docker NAME` runs `docker logs --tail N -f NAME` for live view and
  `docker logs NAME` for search. stdout and stderr are merged.
- Files inside a container are listed with `docker exec NAME ls` and read with
  `cat` / `tail`, so the image needs those tools. Zip files inside containers are not supported.
- Set `AXTAIL_DOCKER` to use another binary, for example `AXTAIL_DOCKER=podman`.

## Limitations in 0.1.0

- Wildcards are supported in the file name only, not in directory names, and directories
  are not scanned recursively.
- Only stored and deflate zip entries are supported; encrypted entries are skipped.
- Positions in compressed and Docker logs are shown as byte offsets, without line numbers.
