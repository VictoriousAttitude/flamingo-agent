# FlamingoAgent

A Windows background service written in Rust that, every 5 seconds, samples the current UTC
time and its own resident memory (RSS), launches a small C++ child with those values, and
keeps the child's log file readable only by Administrators and SYSTEM. The core is portable;
Windows is the fully implemented platform.

```
.
├── agent-rust/       Rust service: install mode, ACL setup, metric loop, child spawning
├── logger-child/     C++17 child: logs its arguments to stdout and an ACL-protected file
├── install.ps1       Build both, install under Program Files, register the service
├── docs/design.md    Design document (architecture, security notes, test strategy)
└── .github/          CI: Linux + Windows on every push
```

## Confirmed interpretations

The brief allowed two readings in three places; these were raised with the assignment
author and confirmed:

| Topic | Decision |
|---|---|
| Child lifecycle | A fresh child per 5-second cycle, receiving the current metrics as arguments; it logs once and exits. The cadence lives in the agent. |
| "Administrator privileges" | As a service the agent runs as LocalSystem, so the child inherits the SYSTEM token and no elevation code runs. Run from a shell, the agent self-elevates once via UAC. |
| Cross-platform reach | Metric collection and child spawning are fully portable. On Linux/macOS the log restriction maps to a root-owned `0600` file and `--install` reports "unsupported" (systemd/launchd registration is the documented next step). |

## Prerequisites (Windows)

- Rust stable with the default MSVC toolchain: https://rustup.rs
- Visual Studio Build Tools 2022 with the **Desktop development with C++** workload (includes CMake)

Nothing else. Both binaries link the C runtime statically, so no Visual C++ redistributable
is needed on the target machine.

## Build and install

From an **elevated** PowerShell in the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

The script builds `flamingo-agent` (`cargo build --release`) and `logger-child` (CMake,
Release, x64), copies both to `C:\Program Files\FlamingoAgent\`, registers the
`FlamingoAgent` service (automatic start, LocalSystem) via `flamingo-agent.exe --install`,
starts it, and prints its status. Re-running the script reinstalls cleanly.

To remove: `powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall`
(or `flamingo-agent.exe --uninstall`). Logs are kept.

## Running interactively

```powershell
& "C:\Program Files\FlamingoAgent\flamingo-agent.exe"
```

From a non-elevated shell this triggers one UAC prompt; the elevated instance runs the same
loop in its own console window and stops on Ctrl-C. Declining the prompt exits with code 3.

Useful flags: `--period-secs`, `--child-timeout-secs`, `--child-path`, `--log-dir`,
`--log-level`. Run `flamingo-agent.exe --help` for details.

## Logs

Both files live in `C:\ProgramData\FlamingoAgent\` (Linux: `/var/log/flamingo-agent/`).

`agent.log` — one `metrics` line and one child outcome per cycle, plus the effective ACL at
start-up:

```
2026-09-11T20:35:00.101Z  INFO flamingo-agent starting mode="service" protection=D:PAI(A;;FA;;;BA)(A;;FA;;;SY) ...
2026-09-11T20:35:00.102Z  INFO metrics utc=2026-09-11T20:35:00.102Z rss_bytes=8421376
2026-09-11T20:35:00.140Z  INFO child completed child_stdout="2026-09-11T20:35:00.102Z rss_bytes=8421376 elevated=true"
```

`child.log` — one line per cycle, written by the child:

```
2026-09-11T20:35:00.102Z rss_bytes=8421376 elevated=true
```

The `elevated=` field is the child's own report of its token, so the "launched with
administrator privileges" requirement is visible in the data, not just asserted here.

## How the ACL is enforced

The agent creates `child.log` (and its directory) **with** a security descriptor attached to
the create call, so the file is never observable with default permissions. The DACL is
expressed as SDDL:

| Object | SDDL |
|---|---|
| `child.log` | `D:P(A;;FA;;;BA)(A;;FA;;;SY)` |
| `FlamingoAgent\` | `D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)` |

`P` disables inheritance, which is the part that is easy to miss: without it the entries
inherited from `ProgramData` (which grant `Users` read access) would remain in effect. The
directory is locked as well because the right to delete or rename a file comes from its
parent directory, not from the file's own ACL. The DACL is re-applied immediately before
every child spawn, so the file is explicitly protected at the moment each child runs even if
an administrator removed it in between. The child only ever appends and never recreates the
file.

## Testing

Portable code is tested automatically on Linux and Windows in CI; Windows-specific behavior
(DACL application, elevation check) has unit tests that run in the Windows CI job, which
executes as an administrator.

```bash
cd agent-rust && cargo test          # unit + integration tests (uses fixture children)
cmake -S logger-child -B logger-child/build && cmake --build logger-child/build && ctest --test-dir logger-child/build
```

What the tests cover: timestamp format, RSS availability, argument encoding, CLI validation,
path resolution, `0700`/`0600` protection on Unix, protected-DACL application on Windows,
child spawn/timeout/failure/cancellation paths, the loop surviving errors and panics, and
the child's exit codes and append-only behavior.

### Coverage and test tiers

Measured line coverage is 87.68% (Linux, `cargo llvm-cov --all-targets`). CI enforces a floor
of 84% (`floor(87.68) - 3`) on the `linux` job and publishes the full `lcov.info` as a build
artifact, so a coverage regression fails the build rather than being noticed later.

The tests fall into three tiers:

1. **Portable unit and integration tests**, run on every commit on both Linux and Windows.
   These include property-based tests of the Windows argument-quoting logic, which check
   quoting round-trips through a reference command-line parser and, on the Windows job, also
   against the real `CommandLineToArgvW`.
2. **Privileged tests**: on Linux, a `sudo`-run job on the CI runner exercises the
   root-owned `0600` log path and a full interactive run of the agent that is stopped with
   `SIGINT`; these now fail the build on CI if the job is not actually root, rather than
   silently reporting `skipped`. On Windows, the DACL tests — born-locked creation,
   replacing an inherited ACL, recovering a file whose ACL denies write, and creating
   missing parent directories — run under any Windows account, because the test process
   owns every file it creates; the one test that actually needs elevation,
   `is_privileged_is_true_on_an_elevated_runner`, asserts that the CI runner's token is
   elevated.
3. **The end-to-end job**, which installs the real Windows service and inspects it live.

Excluded by nature, because they need a live SCM or an interactive desktop session and cannot
be produced from a script: the UAC consent prompt itself, `ServiceMain` running under a real
Service Control Manager, and the SCM's state-polling loops. These remain on the Windows VM
checklist below.

### Windows verification checklist

Run on a clean Windows 11 or Server 2022 machine after `install.ps1`:

| # | Step | Expected |
|---|---|---|
| 1 | `sc qc FlamingoAgent` | `START_TYPE : 2 AUTO_START`, `SERVICE_START_NAME : LocalSystem` |
| 2 | `sc query FlamingoAgent` | `STATE : 4 RUNNING` |
| 3 | `Get-Content $env:ProgramData\FlamingoAgent\agent.log -Wait` | a `metrics` line and a child outcome every 5 s |
| 4 | `Get-Content $env:ProgramData\FlamingoAgent\child.log` | one line per 5 s ending in `elevated=true` |
| 5 | `icacls $env:ProgramData\FlamingoAgent\child.log` | only `BUILTIN\Administrators:(F)` and `NT AUTHORITY\SYSTEM:(F)`, no `(I)` entries |
| 6 | as a standard user: `type C:\ProgramData\FlamingoAgent\child.log` | `Access is denied.` |
| 7 | as a standard user: `del` / `ren` the file | `Access is denied.` |
| 8 | `Stop-Service FlamingoAgent` | returns in < 5 s; no `logger-child` process remains |
| 9 | rename `logger-child.exe` away for 15 s, then back | agent.log shows `child could not be spawned` each cycle, service stays RUNNING, then recovers |
| 10 | run `flamingo-agent.exe` from a non-admin shell | UAC prompt; Ctrl-C stops it cleanly; declining exits 3 |
| 11 | reboot | service is RUNNING without intervention |
| 12 | `install.ps1` again | reinstalls cleanly, still one service |
| 13 | `flamingo-agent.exe --uninstall` | `sc query` reports the service does not exist |

## Verification evidence

The evidence below is the real output of the `windows-service` job of CI run
[34692877714](https://github.com/VictoriousAttitude/flamingo-agent/actions/runs/34692877714)
(commit `c4a9814`), which installs the service on a `windows-latest` runner (the runner
account is an administrator) with `install.ps1`, lets it run, inspects it, then stops and
removes it. Every block below is pasted from that one run. It is not a hand-run VM session;
the items that genuinely need one are listed as not executed at the end of this section.

**1. Registered configuration — `sc.exe qc FlamingoAgent`**

```
[SC] QueryServiceConfig SUCCESS

SERVICE_NAME: FlamingoAgent
        TYPE               : 10  WIN32_OWN_PROCESS
        START_TYPE         : 2   AUTO_START
        ERROR_CONTROL      : 1   NORMAL
        BINARY_PATH_NAME   : "C:\Program Files\FlamingoAgent\flamingo-agent.exe"
        LOAD_ORDER_GROUP   :
        TAG                : 0
        DISPLAY_NAME       : Flamingo Agent
        DEPENDENCIES       :
        SERVICE_START_NAME : LocalSystem
```

**2. Three consecutive cycles — `agent.log`**

Five seconds apart, each with a child that reported `elevated=true`:

```
2026-09-12T12:13:03.927304Z  INFO metrics utc=2026-09-12T12:13:03.927Z rss_bytes=12992512
2026-09-12T12:13:03.956636Z  INFO child completed child_stdout=2026-09-12T12:13:03.927Z rss_bytes=12992512 elevated=true
2026-09-12T12:13:08.928670Z  INFO metrics utc=2026-09-12T12:13:08.928Z rss_bytes=13574144
2026-09-12T12:13:08.950124Z  INFO child completed child_stdout=2026-09-12T12:13:08.928Z rss_bytes=13574144 elevated=true
2026-09-12T12:13:13.929313Z  INFO metrics utc=2026-09-12T12:13:13.929Z rss_bytes=13590528
2026-09-12T12:13:13.958513Z  INFO child completed child_stdout=2026-09-12T12:13:13.929Z rss_bytes=13590528 elevated=true
```

**3. The child log's ACL — `icacls C:\ProgramData\FlamingoAgent\child.log`**

Exactly two ACEs, no inherited (`(I)`) entries:

```
C:\ProgramData\FlamingoAgent\child.log BUILTIN\Administrators:(F)
                                       NT AUTHORITY\SYSTEM:(F)

Successfully processed 1 files; Failed processing 0 files
```

**4. Clean stop and uninstall**

```
stopped in 85 ms
FlamingoAgent removed
```

The service reports `StopPending` from inside the control handler, before it starts draining,
so `Stop-Service` sees the transition immediately instead of waiting for the SCM to poll.

The job additionally asserts, and fails if not: no `logger-child` process survives the stop,
`--uninstall` exits 0, and `Get-Service FlamingoAgent` afterwards returns nothing. A final
step then reinstalls the service, checks it reports `RUNNING` again, and uninstalls it once
more, so "re-running the script reinstalls cleanly" is tested rather than asserted.

**5. Uninstalling when nothing is installed**

After the reinstall above uninstalls the service once more, `--uninstall` is run a further
time against a service that no longer exists, exercising the `ERROR_SERVICE_DOES_NOT_EXIST`
branch against the real SCM:

```
exit 1 : flamingo-agent: FlamingoAgent is not installed
```

**Not executed yet (needs an interactive Windows session):**

- The UAC prompt on an interactive launch from a non-elevated shell (accept, decline → exit 3).
- Standard-user access denial: `runas /user:tester "cmd /c type ...\child.log"` → `Access is denied.`
- Start after a reboot (`Restart-Computer`, then `sc query FlamingoAgent` → RUNNING).

A CI runner has no interactive desktop session, so UAC cannot be exercised there, and the
job cannot reboot the machine it runs on.

## Verified / not verified

- **CI (GitHub Actions, on every push):** `.github/workflows/ci.yml` runs a Linux job (rustfmt, clippy with warnings denied, unit and integration tests, the child's CTest suite, and a compile check of every Windows code path via the `x86_64-pc-windows-gnu` target) and a Windows job (clippy, unit and integration tests under MSVC with a static CRT, and the child's CTest suite). CI proves compilation and tests on both operating systems. It does not exercise UAC elevation or start-on-boot, which need an interactive Windows session and are covered by the checklist above. A third job installs the service through `install.ps1` on the Windows runner, verifies the registered configuration, the log output, and the exact ACL on `child.log`, then stops and uninstalls it. The Linux job also runs the root-level tests under `sudo` (failing the build if that job is not actually root) and enforces an 84% line-coverage floor via `cargo llvm-cov`.
- **Linux:** unit, integration and child tests pass; an interactive run under `sudo`
  produces a root-owned `0600` log.
- **Windows:** the end-to-end CI job above; the interactive-session items (UAC,
  standard-user denial, reboot) are listed as not executed.
- **macOS:** compiles; not executed.

## Design notes

See [`docs/design.md`](docs/design.md). The five points most worth knowing:

1. **Protected DACL, directory included** — see "How the ACL is enforced".
2. **No elevation code in service mode** — a LocalSystem service already holds the most
   privileged token; UAC applies only to interactive sessions.
3. **Cycle isolation** — each cycle runs as its own task with a bounded child wait; an error
   or even a panic in one cycle is logged and the next tick still runs.
4. **Static CRT** — both binaries run on a machine without the VC++ redistributable.
5. **Platform boundary** — every OS call lives in `platform/` or `service/`; business logic
   contains no `cfg`.

## Limitations and next steps

- No log rotation; both logs grow unbounded.
- No Windows Event Log integration; the SCM records service-specific exit codes in the
  System log, which covers start-up failures.
- No single-instance guard; running the service and an interactive copy together interleaves
  two writers.
- Binaries are unsigned; SmartScreen may warn on first interactive launch.
- Linux/macOS: `--install` is not implemented. The mapping is a systemd unit
  (`Type=simple`, `ExecStart=/opt/flamingo/flamingo-agent`, `User=root`) or a launchd
  daemon plist; the agent loop already runs unchanged under either.
