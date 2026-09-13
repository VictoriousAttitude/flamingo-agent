# FlamingoAgent — Design

Windows background service in Rust that samples two metrics every 5 seconds, launches a
privileged C++ child with those metrics, and keeps the child's log readable only by
Administrators and SYSTEM. Portable core, Windows fully implemented, POSIX mapped.

Written 2026-09-11 before implementation and kept current through the hardening work of
2026-09-13. Status: implemented; every mitigation and test named below exists in the tree.

---

## 1. Scope and confirmed interpretations

The brief leaves three points open. All three were raised with the assignment author and
the following readings were confirmed as intended:

| Topic | Decision |
|---|---|
| Child lifecycle | A **fresh child per cycle**. The agent spawns the child every 5 s with the current metrics as arguments; the child logs once and exits. The 5 s cadence lives in the agent. |
| "Administrator privileges" | In **service mode** the agent runs as LocalSystem, so the child inherits the SYSTEM token and no elevation code runs. In **interactive mode** the agent self-elevates once via UAC; everything it spawns inherits the elevated token. |
| Cross-platform reach | The metric/spawn core is fully portable. Windows is the only fully wired platform. On POSIX the log restriction maps to a root-owned `0600` file and `--install` exits with a clear "unsupported" error. systemd/launchd registration is documented as the next step, not implemented. |

Additional decisions stated to the author and accepted:

- The agent **pre-creates and locks** the log file before the child ever runs. The child only appends.
- The memory metric is the **agent's own process RSS** (Working Set on Windows).
- The child is **C++17**, built with CMake, standard library only.

**Non-goals:** log rotation, code signing, remote reporting, configuration files. Each is
noted in the README as a known limitation or next step. (A single-instance guard and
Application event log reporting were originally non-goals and were added later: see §11.1
vector 18 and §4.3.)

---

## 2. Requirements → design map

| Requirement (brief) | Where satisfied |
|---|---|
| Runs as a background service | §4.1–4.3 SCM integration via `windows-service` |
| Two metrics every 5 s: UTC time, process RSS | §5.1 timing, §5.2 metrics |
| Launch child with admin privileges, metrics as string args | §4.4 elevation, §5.3 child contract |
| ACL on child log: only Administrators + SYSTEM | §6 secure log |
| Cross-platform via `cfg` and portable crates | §3 layout, §10 matrix |
| Must not crash on metric or spawn failure | §5.4 isolation guarantees, §9 error policy |
| Child: args in, log to stdout + file every 5 s, respects ACL | §12 child design (cadence is the agent's) |
| `--install` registers `FlamingoAgent`, automatic start | §7 install/uninstall |
| Without `--install`: background daemon | §4.1 run-mode dispatch |
| `install.ps1`: build both, register service | §13 |
| README: build, install, test | §14.6 verification narrative, §17 outline |

---

## 3. Repository layout

```
.
├── agent-rust/                  Rust crate (lib + thin bin), package `flamingo-agent`
│   ├── Cargo.toml, Cargo.lock
│   ├── rust-toolchain.toml      pinned toolchain (1.98.1) and components
│   ├── deny.toml                cargo-deny policy: advisories, license allow-list, sources
│   ├── .cargo/config.toml       static CRT for *-windows-msvc
│   ├── .cargo/mutants.toml      cargo-mutants policy: Windows-only files and named exclusions
│   ├── src/
│   │   ├── main.rs              parse CLI → dispatch (install | uninstall | run)
│   │   ├── lib.rs               module tree; everything below is testable from tests/
│   │   ├── cli.rs               clap definitions + defaults
│   │   ├── config.rs            resolved runtime config (paths, period, timeout)
│   │   ├── app.rs               bootstrap order, interactive entry point, privilege gate
│   │   ├── metrics.rs           UTC timestamp + own RSS                        [portable]
│   │   ├── cycle.rs             one collection cycle: collect → log → spawn     [portable]
│   │   ├── agent.rs             the tick loop + cancellation                   [portable]
│   │   ├── child.rs             argv building, spawn, bind, timeout, outcome   [portable]
│   │   ├── logging.rs           tracing setup (file + optional stderr)         [portable]
│   │   ├── platform/
│   │   │   ├── mod.rs           `#[cfg]` selects one impl; single shared signature set
│   │   │   ├── windows.rs       DACLs, pre-planting checks, job object, instance lock,
│   │   │   │                    elevation check and UAC relaunch, paths
│   │   │   ├── winquote.rs      Windows command-line quoting for the UAC relaunch
│   │   │   └── unix.rs          0700/0600 + owner checks, flock, PDEATHSIG, euid, paths
│   │   └── service/
│   │       ├── mod.rs           `#[cfg]` selects; errors shared by both impls
│   │       ├── unsupported.rs   non-Windows stub: install/uninstall report Unsupported
│   │       ├── windows.rs       dispatcher, ServiceMain, control handler, install/uninstall,
│   │       │                    recovery policy
│   │       └── eventlog.rs      Application event log source: report, register, unregister
│   └── tests/                   integration tests using fixture children (tests/fixtures/):
│                                app_run, child_spawn, cli_run, cycle_run, metrics_first_call,
│                                privileged_linux (root-level, `#[ignore]`)
├── logger-child/                C++17 child, CMake, CTest
│   ├── CMakeLists.txt
│   ├── src/main.cpp
│   └── tests/*.cmake            append, default path, exit codes, expected elevation
├── scripts/soak_check.py        checks a soak run's logs (cadence, outcomes, RSS growth)
├── install.ps1                  build both, install into Program Files, register service
├── README.md                    build, install, evidence, architecture diagrams
├── docs/design.md               this document
└── .github/workflows/
    ├── ci.yml                   four jobs: Linux, mutation testing, Windows, Windows service end-to-end
    └── soak.yml                 manually started long run on both operating systems
```

**Platform boundary rule.** `platform/mod.rs` exposes one set of function signatures; the
Windows and Unix files implement the same names. Only `platform/` and `service/` contain
`#[cfg]` or `unsafe`. Business logic (`metrics`, `cycle`, `agent`, `child`) never sees either.
This is the same pattern `std` uses internally and is what makes the cross-platform claim
structural rather than cosmetic.

---

## 4. Runtime model

### 4.1 Three run modes, one binary

```
flamingo-agent --install     register service (auto start, LocalSystem), then start it
flamingo-agent --uninstall   stop if running, delete the service
flamingo-agent               run the agent loop
```

With no flags the binary must decide whether it was launched **by the SCM** or **by a person**.
It calls `service_dispatcher::start("FlamingoAgent", ffi_service_main)`. If the SCM is the
parent, this call blocks for the service's lifetime and returns only at shutdown. If a person
launched it, the call fails immediately with OS error **1063**
(`ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`); that specific error is the signal to run
interactively. Any other error is a real failure and is reported.

On POSIX there is no dispatcher; the bare invocation always runs interactively (as a daemon
would under systemd).

### 4.2 Service mode: threads and ownership

```
SCM ──starts──▶ process main thread
                  └─ service_dispatcher::start()  ──▶ blocks; SCM calls ServiceMain on a new thread
                                                        │
                                              ServiceMain thread
                                                ├─ register control handler (closure; runs on SCM's thread)
                                                ├─ report StartPending (wait_hint 10 s)
                                                ├─ bootstrap: secure dir → file log → secure child.log (§4.6)
                                                ├─ build tokio runtime (multi_thread, 2 workers)
                                                ├─ report Running (accepts STOP | SHUTDOWN)
                                                ├─ runtime.block_on(agent::run(cfg, cancel))
                                                ├─ report StopPending (wait_hint 10 s)
                                                ├─ drop runtime (bounded by shutdown_timeout 5 s)
                                                └─ report Stopped (exit code 0, or ServiceSpecific on failure)
```

The control handler and the agent loop communicate through one
`tokio_util::sync::CancellationToken`. The handler is `Send + Sync`, does no work, and
returns immediately: on `Stop` or `Shutdown` it cancels the token and returns `NoError`;
on `Interrogate` it returns `NoError`; on anything else `NotImplemented`. Doing real work
inside the handler would block the SCM's thread and is a classic service bug.

### 4.3 Status reporting

The SCM expects state transitions inside the declared `wait_hint`. The agent reports
`StartPending` before building the runtime, `Running` only once the loop is actually
scheduled, `StopPending` the moment cancellation is observed, and `Stopped` after the
runtime has drained. Any panic escaping `ServiceMain` is caught at the boundary and reported
as `Stopped` with a service-specific exit code so the SCM never sees a hung `StopPending`.

**Application event log.** The service has no console and `agent.log` is readable only by
administrators, so the lifecycle facts are also reported to the Application log under the
source `FlamingoAgent` (`RegisterEventSourceW` / `ReportEventW`): event 1 started (with the
`agent.log` path), 2 stopped on request, 3 failed to start (with the bootstrap error text),
4 stopped after a panic (with the panic message), 5 registered command line did not parse.
Events 1–2 are Information, 3–5 Error. Every report is best-effort: an event log that cannot
be written never changes what the service does. `--install` registers the source under
`HKLM\SYSTEM\CurrentControlSet\Services\EventLog\Application\FlamingoAgent` with
`EventMessageFile` pointing at the installed `flamingo-agent.exe` and `TypesSupported = 7`.
The executable carries its own message table: `build.rs` writes a compiled `.res` resource
(`RT_MESSAGETABLE`, IDs 1–5, each the template `%1`) and hands it to `link.exe`, which
accepts `.res` inputs directly, so no `mc.exe`, `rc.exe` or message DLL is involved; on
non-MSVC targets the script emits nothing. A unit test on the Windows job renders every ID
through `FormatMessageW` from the test binary's own module (it carries the same table).
`--uninstall` removes the key. Proof: the end-to-end job reads events 1, 2 and 3 back with
`Get-WinEvent`, checks that the rendered text is the message itself, and checks the registry
values (§14.5). (The first version borrowed `eventcreate.exe`'s message table, whose entries
1–1000 are also `%1`; the embedded table removes the dependency on that file's location.)

### 4.4 Interactive mode and elevation

Interactive mode runs the identical loop with the cancellation token wired to `ctrl_c()`.
Before the loop starts it enforces privilege, because both the ACL work and `--install`
require it:

- **Windows.** `OpenProcessToken` → `GetTokenInformation(TokenElevation)`. If not elevated,
  relaunch self elevated: `CoInitializeEx` (as the shell API documentation recommends), then
  `ShellExecuteExW` with verb `runas`, the original arguments re-quoted with Windows rules,
  and `SEE_MASK_NOCLOSEPROCESS`. The un-elevated parent then **waits on the elevated process
  and exits with its exit code**, so from the caller's terminal `flamingo-agent` behaves like
  one command even though the work happens in a second, elevated process (which gets its own
  console window; Windows does not let an elevated process attach to a lower-integrity
  console). One UAC prompt; the elevated instance does everything else. If the user declines
  UAC, `ShellExecuteExW` fails with `ERROR_CANCELLED` (1223); the agent prints a clear message
  and exits with code 3.
- **POSIX.** `geteuid() == 0`, else "run with sudo", exit code 3.

`--install` and `--uninstall` go through the same check, since creating a service requires
`SC_MANAGER_CREATE_SERVICE`, which only administrators hold.

### 4.5 Paths

The SCM starts services with `%SystemRoot%\System32` as the working directory, so **no
relative path is ever used**. Everything derives from `std::env::current_exe()`:

| Item | Windows | POSIX |
|---|---|---|
| Child binary | `<exe dir>\logger-child.exe` | `<exe dir>/logger-child` |
| Log directory | `%ProgramData%\FlamingoAgent\` (fallback `C:\ProgramData`) | `/var/log/flamingo-agent/` |
| Agent log | `<log dir>\agent.log` | `<log dir>/agent.log` |
| Child log | `<log dir>\child.log` | `<log dir>/child.log` |

All are overridable: `--child-path`, `--log-dir`, `--period-secs`, `--child-timeout-secs`.
Overrides exist for tests and operators, not for normal deployment.

### 4.6 Bootstrap order and fatal errors

The order matters because the file logger must not create `agent.log` in an unlocked
directory, and because failures before logging exists still need to be observable.

1. Parse CLI, resolve config (§4.5).
2. Any failure before the file logger exists is reported on **stderr** and as the process or
   service exit code. In service mode stderr goes nowhere, which is harmless; interactively
   it shows bootstrap problems immediately.
3. Enforce privilege (§4.4).
4. `secure_dir(log_dir)` — create and lock the directory (§6.1 / §6.2).
5. Add the **file** tracing layer; `agent.log` is now born inside a locked directory.
6. `secure_log(child_log)` — create and lock the child log.
7. Log the resolved config, mode, and effective policy; start the loop.

Steps 4 and 6 are the only fatal bootstrap errors: an agent that cannot secure its log
directory must not run, because the graded security property would be silently violated. In
service mode the agent reports `Stopped` with `ServiceExitCode::ServiceSpecific(code)`,
which is visible in `sc query` and is written by the SCM to the System event log
("terminated with service-specific error"); the error text itself goes to the Application
log as event 3 (§4.3). Interactively it prints the error and exits 1. Everything after step 7 is
non-fatal by construction (§5.4).

---

## 5. The collection cycle

### 5.1 Timing

```
let mut tick = tokio::time::interval(period);            // period = 5 s
tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
loop {
    select! {
        _ = cancel.cancelled() => break,
        _ = tick.tick() => {
            let h = tokio::spawn(cycle::run(ctx.clone(), cancel.clone()));
            match h.await {
                Ok(Ok(()))   => {}
                Ok(Err(e))   => error!(error = %e, "cycle failed"),
                Err(join)    => error!(?join, "cycle panicked; continuing"),
            }
        }
    }
}
```

- The first tick fires immediately, so the first sample is at start, not 5 s later.
- Awaiting the spawned cycle inside the tick arm guarantees **at most one cycle in flight**.
- The child timeout (4 s) is strictly below the period (5 s), so a cycle cannot normally
  overrun. If one does, `Delay` shifts the schedule instead of bursting catch-up ticks.
- Cancellation is checked between cycles and *inside* the child wait, so a stop request
  during a hung child resolves within the child timeout, well inside the SCM wait hint.

### 5.2 Metrics

```rust
pub struct Metrics { pub utc: DateTime<Utc>, pub rss_bytes: u64 }
```

- **UTC time.** `chrono::Utc::now()`, formatted RFC 3339 with milliseconds and a `Z` suffix,
  e.g. `2026-09-11T20:35:00.123Z`. Cannot fail.
- **RSS.** `memory_stats::memory_stats()` → `Option<MemoryStats>`; `physical_mem` is RSS on
  Linux/macOS and the **Working Set** on Windows (the OS's RSS equivalent, sourced from
  `GetProcessMemoryInfo`). `None` maps to `MetricsError::RssUnavailable`.
- **Failure policy.** If RSS is unavailable the cycle logs the error, **skips the child for
  that cycle**, and returns `Err`. The loop continues. The contract with the child stays
  strict (both arguments always present) rather than passing sentinels.

Every successful collection writes one line to the agent log, which is how the
"collected and logged every 5 s" criterion is met independently of the child:

```
2026-09-11T20:35:00.123Z INFO metrics utc=2026-09-11T20:35:00.123Z rss_bytes=8421376
```

### 5.3 Child invocation contract

```
logger-child --utc 2026-09-11T20:35:00.123Z --rss-bytes 8421376 --log-file <abs path>
```

Named arguments: order-independent, self-describing, trivially parsed in C++ with no
library. The log path is passed explicitly so the agent is the single source of truth for
the file it secures, and so the child is testable against a temp file on any OS.

Immediately before every spawn the cycle calls `secure_log(child_log)` again (§6.1). It is
idempotent and cheap, and it guarantees the file carries the explicit protected DACL at the
moment each child runs, even if an administrator deleted the file between cycles. Without
this, a recreated file would be protected only by inheritance from the directory and
`icacls` would show `(I)` entries, which the checklist (§14.4 item 6) treats as a failure.

Spawn details (`tokio::process::Command`):

| Setting | Value | Why |
|---|---|---|
| program | absolute child path | no cwd dependence (§4.5) |
| stdin | null | child must never block on input |
| stdout / stderr | piped, captured | a service has no console; output goes to the agent log |
| `kill_on_drop(true)` | on | dropping the future on timeout kills the child |
| lifetime binding | job object with kill-on-close (Windows); `PR_SET_PDEATHSIG` = `SIGKILL` armed before `exec` (Linux) | a hard kill or a crash of the agent, where nothing is dropped, still terminates the child; a binding failure kills the child immediately (`BindFailed`) |
| wait | `timeout(child_timeout, wait_with_output())` | bounded, < period |

Outcome classification, each logged and none fatal:

| Outcome | Log level | Notes |
|---|---|---|
| exit 0 | INFO | the child's stdout line is included verbatim, so the agent log is evidence that both sinks were written |
| exit ≠ 0 | WARN | stderr included; typical cause: log file not writable |
| timed out | ERROR | child killed via `kill_on_drop` |
| spawn failed | ERROR | binary missing / not executable; loop continues |

No shell is involved anywhere, so argument injection is not a concern.

### 5.4 Never-crash guarantees

Layered, each independent of the others:

1. **Result flow.** Every fallible step returns `Result`; `unwrap`/`expect` are denied by
   Clippy outside tests.
2. **Cycle isolation.** Each cycle is a spawned task; a panic becomes a `JoinError` that is
   logged, and the next tick still runs.
3. **Panic hook.** `std::panic::set_hook` routes any panic text into the agent log before
   unwinding, so nothing is lost even if a panic escapes elsewhere.
4. **Service boundary.** `ServiceMain` catches everything and always reaches `Stopped`.

---

## 6. Secure log file

The security property, stated precisely: *no principal other than members of
BUILTIN\Administrators and NT AUTHORITY\SYSTEM can read, write, delete, or rename
`child.log`, from the instant the file exists.*

### 6.1 Windows

**Policy as SDDL.** Expressing the DACL as a string keeps the policy auditable in one line
and avoids hand-assembling ACEs:

| Object | SDDL | Meaning |
|---|---|---|
| `child.log` | `D:P(A;;FA;;;BA)(A;;FA;;;SY)` | protected DACL; Full Access to Administrators and SYSTEM; nothing else |
| `FlamingoAgent\` dir | `D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)` | same principals; ACEs inherit to children so `agent.log` is covered automatically |

`P` (`SE_DACL_PROTECTED`) is the essential flag: it **blocks inheritance** from
`%ProgramData%`, whose default ACL grants `Users` read. Without `P`, the two explicit ACEs
would be added *on top of* inherited ones and the restriction would be silently incomplete.

**Why the directory is locked too.** Deleting or renaming a file is governed by
`FILE_DELETE_CHILD` on the *parent directory*, not by the file's own DACL. Locking only the
file would leave a standard user able to delete or rename the log they cannot read. Locking
the directory closes that.

**Born locked.** The security descriptor is attached to the *create* call, so there is no
window in which the file exists with default permissions:

```
ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, SDDL_REVISION_1, &psd, ..)
SECURITY_ATTRIBUTES { nLength, lpSecurityDescriptor: psd, bInheritHandle: FALSE }
CreateDirectoryW(dir, &sa)                       // ERROR_ALREADY_EXISTS is fine
CreateFileW(file, GENERIC_WRITE, share RW, &sa, OPEN_ALWAYS, ..)  → close handle
LocalFree(psd)
```

**Idempotent re-apply.** Security attributes are ignored when the object already exists, so
after creation the agent always calls
`SetNamedSecurityInfoW(path, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, .., dacl, ..)`
on both objects. A file left over from an earlier version, or one whose ACL was edited by
hand, is corrected on every start.

**Evidence in the log.** After applying, the agent reads the descriptor back
(`GetNamedSecurityInfoW` → `ConvertSecurityDescriptorToStringSecurityDescriptorW`) and logs
the effective SDDL at INFO. A reviewer can see the enforced policy without running `icacls`.

**Ownership.** The owner is the creating token's default owner: SYSTEM in service mode, and
`BUILTIN\Administrators` for an elevated administrator (Windows sets the default owner of
admin tokens to the group, not the user). Both are already granted by the DACL, and
`CREATOR OWNER` is deliberately absent from the SDDL so ownership confers nothing extra to
anyone else.

**Applied when.** At bootstrap (§4.6) and again immediately before every child spawn (§5.3).

**Pre-planting.** `ProgramData` lets any user create a subdirectory, and a junction needs no
privilege, so an object may already exist at the log path before the agent's first start. An
existing object is adopted only if `GetFileAttributesW` shows no `FILE_ATTRIBUTE_REPARSE_POINT`
and its owner (`GetNamedSecurityInfoW` with `OWNER_SECURITY_INFORMATION`, compared with
`EqualSid` against the well-known Administrators and LocalSystem SIDs) is one of those two.
Otherwise `secure_dir`/`secure_file` return `PlatformError::Untrusted` and bootstrap fails
closed; the agent never takes ownership of a planted object, because a hostile owner keeps the
implicit `WRITE_DAC` that would let them undo the lock afterwards. Proof: a unit test refuses a
junction on the Windows runner, and the end-to-end job plants a directory and a junction as the
standard account and requires the agent to exit 1 for both.

**Privilege required.** `WRITE_DAC` on the object — held implicitly by the owner and by
SYSTEM/Administrators. This is why the agent must be elevated in interactive mode.

### 6.2 POSIX

Same shape, native primitives:

| Object | Mode | Owner | Mechanism |
|---|---|---|---|
| directory | `0700` | `root:root` | `DirBuilder::mode(0o700)` |
| `child.log` | `0600` | `root:root` | `OpenOptions::mode(0o600)` on create (born locked), then `set_permissions` to re-apply |

Ownership needs no `chown`: a new object belongs to the creating user, root under §4.4, and an
existing one owned by anyone else is refused rather than taken over (see the pre-planting
rule below), so root ownership is a consequence rather than a step. (An explicit `chown` was
in the first version; a mutation pass showed it could not be observed by any test, which is
what pointed out that it had become unreachable.) Tests that run unprivileged exercise mode
bits against a temp dir; the root-level tier asserts the owner. macOS is the same code path;
it is compiled but not exercised in this deliverable.

The same pre-planting rule applies here: an existing path that is a symbolic link
(`symlink_metadata`) or is owned by a uid other than the effective one is refused with
`PlatformError::Untrusted`. Proof: unit tests refuse a symlinked directory and file; the
root-level test hands a directory to `nobody` and requires the refusal.

### 6.3 The child's obligations

The child opens the file with `std::ios::app` and **never** truncates, deletes, or recreates
it, and never touches permissions. Recreating the file would produce a new object with
inherited defaults and destroy the restriction. This is stated in the child's source and in
the README.

---

## 7. Service installation

`--install` (elevated):

```
ServiceManager::local_computer(None, CONNECT | CREATE_SERVICE)
  .create_service(&ServiceInfo {
      name:            "FlamingoAgent",
      display_name:    "Flamingo Agent",
      service_type:    OWN_PROCESS,
      start_type:      AutoStart,
      error_control:   Normal,
      executable_path: current_exe(),          // the copy in Program Files
      launch_arguments: [],                    // bare invocation = service mode (§4.1)
      dependencies:    [],
      account_name:    None,                   // LocalSystem
      account_password: None,
  }, CHANGE_CONFIG | START)
```

then set the description, start the service, and poll its status for up to 10 s until it
reports `Running`. If it reaches `Stopped` instead, print the service-specific exit code
(§4.6) so a bootstrap failure is visible from the install command itself, not only from
`sc query`. **Idempotency:** if the service already
exists (`ERROR_SERVICE_EXISTS`, 1073) the agent opens it and updates its configuration to
the same values, so re-running `install.ps1` is safe.

**Recovery.** Installation also registers failure actions with the SCM
(`ChangeServiceConfig2` through `update_failure_actions`): restart after 5 s, restart after
5 s again, then stop; the attempt counter resets after a failure-free day. The
"failure actions on non-crash failures" flag is set, so a service that exits with a
service-specific code (a transient bootstrap failure) is retried the same way as one that
crashes. Bounded attempts keep a persistently broken deployment from restarting forever.
Proof: the end-to-end job reads the configuration back with `sc qfailure` and `sc qfailureflag`.

**Event source.** Before the first start, installation registers the `FlamingoAgent` event
source (§4.3) against the installed executable's own message table, so the very first
lifecycle event renders as text; uninstallation removes the registration after deleting the
service.

`--uninstall` (elevated): open with `STOP | QUERY_STATUS | DELETE`; if running, send stop and
poll status up to 15 s; then delete. A service marked for deletion while a handle is open is
removed when the last handle closes; the agent closes its handle immediately.

Both commands print one human-readable line on success and map SCM errors to messages
(access denied → "run as administrator", not found → "service is not installed").

---

## 8. Agent logging

`tracing` with two layers:

- **File layer** (always): `tracing-appender` non-blocking writer to `agent.log`, compact
  format, UTC timestamps, no ANSI. The `WorkerGuard` is held until the very end of shutdown
  so the final lines are flushed.
- **stderr layer** (interactive only): same events, human-friendly.

Default level INFO; `--log-level` overrides. Logged at INFO every cycle: the metrics line
(§5.2) and the child outcome (§5.3). Logged once at start: resolved config, effective SDDL
(§6.1), service/interactive mode.

`agent.log` lives in the locked directory and inherits its protection (§6.1); it is not
part of the graded ACL requirement but there is no reason to leave it open.

---

## 9. Error handling policy

| Layer | Type | Rule |
|---|---|---|
| modules (`metrics`, `child`, `platform`, `service`) | `thiserror` enums | precise, matchable variants; carry the OS error |
| `cycle`, `agent`, `main` | `anyhow::Result` with `.context()` | human-readable chains for the log and stderr |
| tests | `unwrap`/`expect` allowed | `#[cfg_attr(test, allow(...))]` |

Lints: `#![deny(unsafe_op_in_unsafe_fn)]` and, outside tests,
`#![deny(clippy::unwrap_used, clippy::expect_used)]`. CI runs `cargo clippy --all-targets
-- -D warnings`, so every default Clippy warning is a build failure. Every `unsafe` block
carries a `// SAFETY:` comment stating the invariant; all of them live in
`platform/windows.rs`, `platform/unix.rs` (one `geteuid` call) and `service/windows.rs`.

Process exit codes: `0` success · `1` runtime failure · `2` usage · `3` insufficient
privilege · `4` unsupported on this platform.

The release profile pins `panic = "unwind"` explicitly. Setting `panic = "abort"` would turn
every panic into immediate process death and silently defeat the cycle isolation in §5.4.
The unit test for a panicking cycle documents the intended behaviour but cannot catch that
regression on its own: `cargo test` builds with the test profile, which always unwinds, so
the `[profile.release]` entry is what actually holds the guarantee in a shipped binary.

---

## 10. Cross-platform matrix

| Capability | Windows | Linux | macOS |
|---|---|---|---|
| Metrics (UTC, RSS) | ✅ Working Set | ✅ RSS | ✅ RSS (compiled, untested) |
| Tick loop, child spawn, timeout | ✅ | ✅ | ✅ |
| Secure log | ✅ protected DACL | ✅ root `0600` | ✅ root `0600` (untested) |
| Privilege check | ✅ token elevation | ✅ euid 0 | ✅ euid 0 |
| Self-elevate | ✅ UAC via `runas` | ✗ error: use sudo | ✗ error: use sudo |
| Service registration | ✅ SCM | ✗ exit 4, systemd unit documented | ✗ exit 4, launchd documented |
| Child binary | ✅ MSVC, static CRT | ✅ g++/clang | ✅ clang (untested) |

---

## 11. Security notes and Windows background

### 11.1 Threat model

**Attacker.** A local standard user on the target machine: no administrator rights, no
physical access, no kernel or driver, but able to run programs, create files under
`ProgramData` and `Users\Public`, create junctions and symbolic links to places they can
already reach, and kill or start their own processes. **Out of scope:** a local administrator
(who can undo any DACL by design), a remote attacker (the agent opens no network surface), a
compromised SYSTEM account or kernel, and hardware or offline-disk access.

**Assets.** The confidentiality and integrity of `child.log` (the graded asset), the
integrity of the two binaries, and the availability of the service.

| # | Vector | Mitigation | Proof |
|---|---|---|---|
| 1 | Read `child.log` | Protected DACL grants only Administrators and SYSTEM; the directory is locked too, so traversal is denied | `secure_file_creates_born_locked`; end-to-end "Standard user is denied", README block 7 |
| 2 | Delete or rename `child.log` | `DELETE` withheld by the file DACL and `FILE_DELETE_CHILD` withheld by the directory DACL | end-to-end delete/rename denied, block 7 |
| 3 | Modify or truncate `child.log` | Same DACL; the child only appends and never recreates the file | `icacls` block 3; `secure_file_replaces_inherited_acl_and_keeps_content` |
| 4 | Regain access through inheritance | `P` (protected) DACL: no inherited entries survive | `assert_file_policy` tests; "no `(I)`" assertion, block 3 |
| 5 | Regain access between cycles (delete and recreate the file) | DACL re-applied immediately before every spawn | `dacl_is_reapplied_after_the_log_is_deleted` |
| 6 | Pre-plant the log directory before first start (foreign owner keeps `WRITE_DAC`) | An existing directory or file owned by anyone but Administrators/SYSTEM (root/euid on Unix) is refused; ownership is never taken over | end-to-end "Planted log locations are refused"; `foreign_owned_log_directory_is_refused` (root) |
| 7 | Pre-plant a junction or symbolic link (redirect SYSTEM's writes) | Reparse points and symlinks are refused before any write | `secure_dir_refuses_a_junction`; `secure_dir_refuses_a_symbolic_link`; end-to-end planted link |
| 8 | Leave a weakened DACL that denies the agent write access | Re-apply falls through `CreateFileW`'s denial to `SetNamedSecurityInfoW`, which the owner may always call | `secure_file_recovers_a_file_that_denies_write` |
| 9 | Plant a binary or DLL the agent will load | Child path is absolute under `Program Files` (administrators only); no `PATH` search; both binaries import only OS DLLs (static CRT) | end-to-end import-table check, block 10 |
| 10 | Inject arguments through the metric values | No shell anywhere; argv passed directly; quoting exists only for the UAC relaunch and is property-tested against `CommandLineToArgvW` | `quoting_round_trips_through_the_reference_parser`; `round_trips_through_the_real_parser` |
| 11 | Remove or replace the child binary | `Program Files` is administrator-only; a missing child is logged each cycle and the service keeps running | end-to-end missing-child step, block 6 |
| 12 | Make the child hang or flood its output | Bounded wait with `kill_on_drop`; pipes drained concurrently with the wait | `slow_child_times_out_and_is_killed`; `large_stdout_does_not_deadlock` |
| 13 | Orphan a child by killing the agent | Job object with kill-on-close (Windows); parent-death signal (Linux) | `job_object_kills_its_processes_when_closed`; `hard_killed_agent_takes_its_child_with_it`; end-to-end hard-kill step |
| 14 | Crash the agent through a bug in a cycle | Each cycle is its own task; a panic is logged and the loop continues; the SCM restarts the service after a real crash | `a_panicking_cycle_does_not_stop_the_loop`; `sc qfailure` end-to-end step |
| 15 | Abuse the interactive relaunch to elevate something else | The relaunch targets `current_exe()` only; refusals return an error code instead of blocking on a dialog | end-to-end UAC decline, block 8 |
| 16 | Stop the service or edit its registration | Requires SCM `STOP`/`CHANGE_CONFIG` rights, held by administrators only (Windows semantics, not agent code) | `sc qc`, block 1 |
| 17 | Exhaust the disk through log growth | **Not mitigated**: no rotation. Listed in README limitations | — |
| 18 | Interleave logs by starting a second instance | An exclusive lock on `<log dir>/agent.lock` (no-share open on Windows, `flock` on Unix) is taken before the file logger opens; a second instance exits with code 5 and writes nothing | `second_instance_on_the_same_directory_is_refused` (both platforms); `second_agent_on_the_same_log_directory_exits_5` (root); end-to-end "Second instance" step |

The one unmitigated row is an availability concern for an administrator, not a
confidentiality or integrity loss to the standard user, which is why it was left as a
documented follow-up rather than built.

### 11.2 Windows facts the design depends on

These are stated so a reviewer can check the reasoning.

- **Session 0 and services.** Since Vista, services run in session 0 with no interactive
  desktop. UAC is a property of interactive logon sessions; it does not apply to services.
  A LocalSystem service holds a full, unfiltered token. Calling `runas` from inside it is
  meaningless, which is why the service path contains no elevation code.
- **Full vs. filtered tokens.** An interactive administrator normally runs with a *filtered*
  token in which the Administrators group is marked deny-only. `TokenElevation` reports
  whether the current token is the full one. Relaunching through `ShellExecuteW("runas")`
  is the supported way to obtain it; `CreateProcess` cannot elevate.
- **Well-known SIDs.** `BA` = `S-1-5-32-544` (BUILTIN\Administrators), `SY` = `S-1-5-18`
  (NT AUTHORITY\SYSTEM). Using SDDL aliases avoids hard-coding SID strings.
- **DACL evaluation.** ACEs are evaluated in order; an empty DACL denies everyone, a *null*
  DACL grants everyone. The design always sets a non-null DACL with explicit allow ACEs and
  no deny ACEs, so order is irrelevant.
- **Inheritance.** `SE_DACL_PROTECTED` stops parent ACEs from flowing in. `OI`/`CI` on the
  directory's ACEs make them flow to new files/subdirectories. `icacls` shows inherited
  entries with `(I)`; the graded file must show none.
- **Delete rights.** `DELETE` on the object *or* `FILE_DELETE_CHILD` on the parent. Both are
  closed (§6.1).
- **Child lifetime.** `kill_on_drop` only runs when the agent shuts down normally. Each child
  is therefore also placed in a job object with kill-on-close; the job handle is held for the
  agent's whole life, so the kernel closes it when the agent dies for any reason and terminates
  every process still in the job, including anything the child started. On Linux the child arms
  `PR_SET_PDEATHSIG` with `SIGKILL` between `fork` and `exec`, then re-checks its parent pid,
  because the parent can die before the flag is set. Proof: a root-level Linux test and an
  end-to-end CI step hard-kill the agent and assert the child is gone.
- **Binary planting.** The child path is absolute and under `Program Files`, writable only by
  administrators. The agent never searches `PATH` and never uses a shell.
- **Static CRT.** Both binaries link the C runtime statically (`+crt-static` for Rust,
  `MultiThreaded` runtime for MSVC) so a clean machine without the VC++ redistributable can
  run them. A dynamically linked child failing with a missing DLL would look like a spawn
  error and would be an easy way to fail the "clean environment" criterion.
- **Unsigned binaries.** SmartScreen may warn on first interactive launch. Not addressed;
  documented.

---

## 12. Child binary (`logger-child`, C++17)

```
logger-child --utc <rfc3339> --rss-bytes <n> [--log-file <path>]
```

- **Parsing.** A loop over `argv` matching the three flags; both metrics required; `--log-file`
  defaults to the platform path via `#ifdef _WIN32`.
- **Privilege probe.** The child reports whether it is actually running elevated, so the
  "launched with elevated privileges" criterion has evidence in the log rather than an
  assertion in the README. `#ifdef _WIN32`: `OpenProcessToken` + `GetTokenInformation(TokenElevation)`;
  otherwise `geteuid() == 0`. This and the default path are the child's only platform-specific
  code, each isolated in one small function.
- **Output.** One line, identical on both sinks: `<utc> rss_bytes=<n> elevated=<true|false>`.
  Written to `stdout` and appended to the log file (`std::ofstream(path, std::ios::app)`),
  flushed, and the stream state checked.
- **Exit codes.** `0` ok · `1` usage · `2` log file could not be opened or written (message
  on stderr). The agent surfaces code 2 as a WARN with the stderr text, which is exactly what
  an ACL misconfiguration would look like in the agent log.
- **Cadence.** None. The child logs once and exits; the agent provides the 5 s rhythm. This
  is the confirmed interpretation and is stated in the README.
- **Portability.** Standard library for everything but the privilege probe: `<filesystem>`,
  `<fstream>`, `<iostream>`, `<string>`. The two `#ifdef _WIN32` sites are the default log
  path and the probe; nothing else knows which OS it is on.
- **Build.** CMake ≥ 3.15, `CMAKE_CXX_STANDARD 17`, `/W4 /permissive-` on MSVC,
  `-Wall -Wextra` elsewhere, static MSVC runtime (§11). CTest runs the binary against a temp
  file and asserts exit code and content, so the child has an automated test on every OS.

---

## 13. Build and install script (`install.ps1`)

```
#Requires -RunAsAdministrator
Set-StrictMode -Version Latest; $ErrorActionPreference = 'Stop'
```

1. Check prerequisites: `cargo`, `cmake`; fail with an actionable message naming the
   missing tool and where to get it.
2. `cargo build --release --manifest-path agent-rust\Cargo.toml`
3. `cmake -S logger-child -B logger-child\build -A x64` ; `cmake --build ... --config Release`
4. Copy both executables to `$env:ProgramFiles\FlamingoAgent\`.
5. If the service already exists, run `--uninstall` first (clean reinstall).
6. Run `flamingo-agent.exe --install`; then `sc query FlamingoAgent` and print the state and
   the log directory.

`-Uninstall` switch: stops and removes the service and the install directory. Logs under
`%ProgramData%\FlamingoAgent` are deliberately left in place. Any failure exits non-zero with
the failing step named.

All paths inside the script resolve from `$PSScriptRoot`, so it works from any current
directory. The README shows the invocation that sidesteps a restrictive execution policy on
a fresh machine: `powershell -ExecutionPolicy Bypass -File .\install.ps1`.

Prerequisites on the target machine, documented in the README: Rust stable (MSVC toolchain),
Visual Studio Build Tools 2022 with the C++ workload (which bundles CMake). Nothing else.

---

## 14. Testing strategy

Principle: everything portable is tested automatically on Linux and Windows; everything
Windows-specific is verified against the written checklist (§14.4) by the end-to-end CI job
on a real Windows runner, with its output pasted into the README; anything not exercised is
named as such.

### 14.1 Unit tests (`cargo test`, any OS)

| Module | Cases |
|---|---|
| `metrics` | RFC 3339 format ends in `Z`; RSS is at least 1 MiB on this platform (a constant reading would pass `> 0`); the first `memory_stats` call is serialized across threads (`metrics_first_call`) |
| `child` | argv builder yields the exact expected sequence; outcome classification for exit 0 / non-zero / timeout / spawn error |
| `cli` | `--install`, `--uninstall`, bare, and every override parse; conflicting flags rejected |
| `config` | path resolution relative to a fake exe location |
| `platform::unix` | on a temp dir: dir `0700`, file `0600`, re-apply fixes a `0644` file; owner asserted by the root-level tier; a planted symbolic link is refused for both the directory and the file; an inspection error other than "not found" surfaces as such; a second instance lock on the same directory is refused until the first is dropped |
| `platform::winquote` | a property-based test (`quoting_round_trips_through_the_reference_parser`) checks quoting round-trips through a reference command-line parser for randomly generated arguments; a fixed set of tricky arguments is additionally checked against the real `CommandLineToArgvW` on the Windows job |
| `platform::windows` | elevation is reported `true` on the elevated CI runner (`is_privileged_is_true_on_an_elevated_runner`); a file whose ACL denies write is recovered by re-applying the DACL (`secure_file_recovers_a_file_that_denies_write`); the log directory is created when its parent does not yet exist (`secure_dir_creates_missing_parents`); a planted junction is refused (`secure_dir_refuses_a_junction`); a kill-on-close job object terminates its processes when the handle closes (`job_object_kills_its_processes_when_closed`); a second instance lock is refused until the first is dropped |
| `app` | the relaunch exit-code mapping and its stderr line; the panic hook writes the panic into the log (captured through a thread-local subscriber); a child timeout not below the period is rejected |
| `service::windows` | the recovery policy's shape (`recovery_policy_restarts_twice_then_gives_up`); status builders; service info; both panic payload forms are rendered |
| `service::eventlog` | an unregistered source can still report; registering and removing the source under HKLM round-trips on an elevated runner |
| `agent` | with period = 50 ms and a counting cycle: N ticks in ~N·50 ms; cancellation stops within one period; a cycle that returns `Err` does not stop the loop; a cycle that **panics** does not stop the loop |

### 14.2 Integration tests (`agent-rust/tests/`, any OS)

Fixture "children" (tiny scripts on Unix, `.cmd` on Windows) stand in for the real child so
the spawn path is tested without a C++ toolchain:

- echoes its args → outcome `exit 0`, stdout captured verbatim
- exits 3 → outcome `non-zero` with stderr captured
- sleeps 10 s → outcome `timed out` within `child_timeout + 200 ms`, and the process is gone
- non-existent path → outcome `spawn failed`
- a full cycle re-applies the DACL to the child log after it is deleted mid-run
  (`dacl_is_reapplied_after_the_log_is_deleted`)
- the whole bootstrap-and-loop path in one process, unprivileged, against a temporary
  directory: cancelled after its first cycles, it must have logged the protection string,
  every cycle and its clean stop (`tests/app_run.rs`)
- the real binary: `--help`, `--version`, conflicting flags as a usage error, `--install`
  unsupported off Windows, and an unprivileged run that must exit 3 within a bounded wait
  (`tests/cli_run.rs`)

Root-only tests live separately in `tests/privileged_linux.rs`, `#[ignore]`d by default so an
unprivileged `cargo test` stays green: a root-owned `0700`/`0600` log path
(`secure_paths_are_root_owned_0600`); a full interactive run of the real binary, stopped
with `SIGINT`, that completes two cycles cleanly (`interactive_run_under_root_completes_two_cycles`);
a second agent on a running agent's log directory exiting 5 without writing to its log
(`second_agent_on_the_same_log_directory_exits_5`); a `SIGKILL`ed agent whose child must be
gone within 3 s (`hard_killed_agent_takes_its_child_with_it`); and a log directory owned by
another user being refused (`foreign_owned_log_directory_is_refused`). CI runs them under
`sudo` as a separate step (§14.5) and fails the build if that step is not actually root.

### 14.3 Child tests (`ctest`, any OS)

- happy path: exit 0, stdout equals `<utc> rss_bytes=<n> elevated=<flag>`, file contains
  exactly that line; `<flag>` is `false` when the test runs unprivileged, `true` under
  sudo/elevation, with the expected flag derived from the process's own privilege rather than
  hard-coded (`expected_elevation`)
- falls back to the platform default log path when `--log-file` is omitted, observed as
  file creation on Windows and as the documented exit code and path on Unix
  (`default_path_test`)
- second run appends (two lines), never truncates
- missing `--rss-bytes` → exit 1
- unwritable `--log-file` (a directory path) → exit 2, message on stderr

### 14.4 Windows VM verification checklist

Run on a fresh Windows 11 or Server 2022 VM after installing the documented prerequisites.

| # | Step | Expected |
|---|---|---|
| 1 | `.\install.ps1` | exit 0; prints `FlamingoAgent: RUNNING` and the log directory |
| 2 | `sc qc FlamingoAgent` | `START_TYPE : 2 AUTO_START`; `SERVICE_START_NAME : LocalSystem`; `BINARY_PATH_NAME` under Program Files |
| 3 | `sc query FlamingoAgent` | `STATE : 4 RUNNING` |
| 4 | `Get-Content $env:ProgramData\FlamingoAgent\agent.log -Wait` | one `metrics` line and one child outcome (exit 0) every 5 s; an `effective DACL` line at start |
| 5 | `Get-Content $env:ProgramData\FlamingoAgent\child.log` (admin) | one line per 5 s, matching the agent's metrics lines, each ending in `elevated=true` |
| 6 | `icacls $env:ProgramData\FlamingoAgent\child.log` | exactly `BUILTIN\Administrators:(F)` and `NT AUTHORITY\SYSTEM:(F)`; **no** `(I)` entries |
| 7 | `net user tester P@ssw0rd! /add` then `runas /user:tester "cmd /c type C:\ProgramData\FlamingoAgent\child.log"` | `Access is denied.` |
| 8 | same user: `cmd /c del ...child.log` and `cmd /c ren ...` | `Access is denied.` Then `net user tester /delete` to leave the VM clean |
| 9 | `Stop-Service FlamingoAgent` | returns in < 5 s; `Get-Process logger-child` finds nothing; agent.log ends with a clean shutdown line |
| 10 | Start service; rename `logger-child.exe` away; wait 15 s; rename back | agent.log shows `spawn failed` each cycle, service stays RUNNING, then recovers |
| 11 | From a non-admin PowerShell: `flamingo-agent.exe` | UAC prompt; accept → runs, Ctrl-C stops cleanly. Decline → clear message, exit code 3 |
| 12 | `Restart-Computer`; after login `sc query FlamingoAgent` | RUNNING without intervention (proves AutoStart, absolute paths, static CRT) |
| 13 | `.\install.ps1` a second time | exit 0; still one service, still RUNNING |
| 14 | `flamingo-agent.exe --uninstall`; `sc query FlamingoAgent` | error 1060 (does not exist); no `logger-child` process |

Every row except the Accept click in row 11 and the reboot in row 12 is executed by the
`windows-service` CI job on every push (§14.5), and the README pastes its output.

### 14.5 Continuous integration (`.github/workflows/ci.yml`)

Runs on every push and pull request. Four jobs, all required to pass:

| Job | Runner | Steps |
|---|---|---|
| `linux` | `ubuntu-latest` | `cargo fmt --check` · `cargo clippy --all-targets -- -D warnings` · `cargo deny check` (advisories, license allow-list, sources; policy in `deny.toml`) · `cargo audit` · `cargo test` · privileged Linux tests under `sudo` (`cargo test --test privileged_linux -- --ignored`) · restore target ownership (`always()`, so it runs even if the previous step failed) · install `cargo-llvm-cov` · coverage lcov (`cargo llvm-cov --all-targets --lcov`) · coverage line with `--fail-under-lines 89` · upload the lcov artifact · compile-check every Windows code path (`cargo check --target x86_64-pc-windows-gnu`) · `cmake` configure/build + `ctest` for the child |
| `windows` | `windows-latest` | `cargo clippy --all-targets -- -D warnings` · `cargo test` (MSVC, static CRT) · `cmake -A x64` build + `ctest` for the child |
| `windows-service` | `windows-latest`, `needs: [windows]` | end-to-end run of the real service (below) |
| `mutants` | `ubuntu-latest` | `cargo mutants -j 2` with the policy in `agent-rust/.cargo/mutants.toml`; fails if any mutant of the portable or Unix code survives; uploads `mutants.out` |

`windows-service` runs only after `windows` is green, so a compile or unit-test failure is
never diagnosed as a service failure. The GitHub runner account is an administrator, which
is what makes the SCM and the ACL reachable from CI at all. The job:

1. installs everything with `install.ps1` exactly as a reviewer would;
2. asserts `sc.exe qc` reports `AUTO_START` and `LocalSystem`, and `sc.exe query` reports `RUNNING`;
3. waits 12 s, then checks `agent.log` shows the 5 s cadence and `child.log` has at least two
   lines, the last ending in `elevated=true`;
4. asserts `icacls child.log` shows exactly two ACEs (Administrators and SYSTEM, both `(F)`)
   and no inherited `(I)` entry;
5. renames `logger-child.exe` away for 12 s and requires the service to stay `RUNNING` while
   logging a spawn failure each cycle, then restores it and requires a completed cycle;
6. creates a local standard user and, in a credentialed process, requires read, delete and
   rename of `child.log` to be denied and the file to be intact afterwards;
7. sets the UAC policy "automatically deny elevation requests" for standard users, launches
   the agent as that user and requires exit code 3 with the blocked-by-policy message — the
   agent's own `runas` relaunch, refused by the Application Information service;
8. launches the agent interactively (the runner's token is already elevated), lets it cycle,
   sends a real console `CTRL_C_EVENT` through a helper attached to its console, and requires
   exit 0, the shutdown line and no surviving child;
9. reads both binaries' import tables with `dumpbin` and requires no VC runtime, C++ standard
   library or Universal CRT DLL;
10. stops the service, requires it to return in under 8 s with no orphaned `logger-child`,
    then uninstalls and requires the registration to be gone;
11. **reinstalls** with `install.ps1` a second time, requires `RUNNING` again, uninstalls once
    more, and finally requires `--uninstall` on the now-missing service to exit 1 with
    "not installed" — the idempotent-reinstall claim is therefore tested, not asserted.

Steps added by the hardening work, in job order:

12. reads the recovery configuration back with `sc qfailure` and `sc qfailureflag` (§7);
13. reads the `FlamingoAgent` event source registration from the registry and the start event
    (ID 1, Information, rendered text) with `Get-WinEvent` (§4.3);
14. as the standard user, plants a directory and a junction where a log directory is
    expected, and requires the elevated agent pointed at each to exit 1 with the refusal
    message (§6.1 pre-planting);
15. launches an interactive agent on the running service's log directory and requires exit 5
    with the already-running message while the service stays `RUNNING` (§11.1 vector 18);
16. kills an interactive agent with `Stop-Process -Force` and requires its child to be gone
    within 3 s (§5.3 lifetime binding);
17. swaps the registered command line for one the config check rejects, starts the service,
    requires the bootstrap-failure event (ID 3, Error, with the error text) and
    `SERVICE_EXIT_CODE 1`, then restores the command line and requires `RUNNING` again;
18. on the clean stop, requires the stop event (ID 2) and, after uninstall, requires the event
    source registration to be gone.

CI therefore proves compilation, unit, integration and child tests on both operating systems,
plus registration, cadence, the exact ACL, standard-user denial, the UAC decline path, a clean
interactive stop and clean-machine startability on a real Windows machine. Two checklist items
remain manual by nature: clicking Accept on the UAC consent dialog (it happens on the secure
desktop) and an actual reboot (a hosted runner cannot restart itself). The README lists exactly
those two as not executed.

The toolchain is pinned to 1.98.1 (`rust-toolchain.toml` and every `dtolnay/rust-toolchain`
step) and every action to a commit SHA; the cross target is added with `rustup target add`.
No secrets, no deployment step.

### 14.6 What is and is not verified

Stated verbatim in the README so the reviewer knows what was actually run:

- **Linux:** unit, integration, and child tests pass; interactive run under `sudo` produces
  a root-owned `0600` log.
- **Windows:** the `windows-service` CI job (§14.5), with its output pasted: `sc qc`,
  `sc qfailure`, `agent.log`, `icacls`, stop/uninstall, missing-child recovery, standard-user
  denial, the UAC decline path, planted log locations refused, a second instance refused, the
  interactive Ctrl+C stop, the hard-kill child cleanup, the event log records and the
  import-table check. The two items that need a human — the UAC Accept click and an actual
  reboot — are listed as not executed.
- **macOS:** compiles; not executed.

### 14.7 Coverage and tiers

Measured line coverage is 92.70% on Linux and 78.00% on Windows (`cargo llvm-cov
--all-targets` on each job, CI run 34763931662). CI enforces a floor of 89% (`floor(92.70) - 3`)
on the `linux` job and 75% (`floor(78.00) - 3`) on the `windows` job, and publishes both lcov files as build
artifacts, so a coverage regression fails the build rather than being noticed later. The
Windows figure is lower because the code that only the end-to-end job exercises (the SCM
wrapper, `ServiceMain`, installation, the UAC relaunch) is measured there but not
instrumented: the end-to-end job runs the installed release binary, not a test build.

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

**Soak.** A manually started workflow (`soak.yml`) runs the real agent for N minutes on
both operating systems (Linux: interactive under `sudo`, 2 s period, stopped with `SIGINT`;
Windows: the installed service at its 5 s cadence) and checks the logs with
`scripts/soak_check.py`: at least 90% of the expected cycles, every one with a completed
child reporting `elevated=true`, no `ERROR`/`WARN` lines, and resident-memory growth between
the post-warm-up steady state and the last tenth of the run bounded by 2 MiB. It answers the
one question the per-push jobs cannot: whether anything drifts over hundreds of cycles.

**Mutation testing.** Coverage is necessary, not sufficient: a line can run without any
assertion depending on it. The `mutants` job (§14.5) runs `cargo mutants` over the portable
and Unix sources, 84 mutants in the current tree, and fails the build if one survives. The
current pass: 72 killed, 12 unviable (they do not compile), 0 missed. Three mutants are
excluded by name in `.cargo/mutants.toml`, each with its reason: `is_privileged` returning
`false` and `prepare_child` doing nothing are killed only by the root-level tier, which that
job does not run; `LOCK_EX ^ LOCK_NB` is equivalent to `LOCK_EX | LOCK_NB` because the two
flags share no bits. The first pass left 13 alive and drove five new tests plus the removal
of an unreachable `chown` (§6.2). The Windows-only files are not mutated; their tests run on
the Windows job without instrumentation.

Not counted by either coverage figure: `ServiceMain` under the
real Service Control Manager and the SCM's state-polling loops, which the end-to-end job
(§14.5) exercises on every push but does not instrument. Two checklist items (§14.4) need a
human at the machine and no script can perform them: clicking Accept on the UAC consent
dialog, and an actual reboot.

---

## 15. Development environment and order of work

### 15.1 Environment

**Linux (development).** Install `rustup` (stable), `cmake`, `mingw-w64`; add target
`x86_64-pc-windows-gnu`. This target compile-checks every `#[cfg(windows)]` path from Linux
(`cargo check --target x86_64-pc-windows-gnu`), catching API mistakes before touching the
VM. It proves compilation only, never behavior.

**Windows VM (verification).** Windows 11 or Server 2022 evaluation image; `rustup` with the
MSVC toolchain; Visual Studio Build Tools 2022 with the C++ workload; git. Snapshot the VM
right after installing prerequisites so every checklist run starts clean.

### 15.2 Milestones — the risk comes first

| # | Milestone | Done when |
|---|---|---|
| M0 | Environments ready | Linux toolchain installed; VM snapshot taken |
| M1 | **Windows spike** | A skeleton agent installs, starts, logs a heartbeat, locks one file with the SDDL policy, and stops cleanly. Checklist items 1–3, 6–9, 14 pass. Nothing portable exists yet. |
| M2 | Portable core | Metrics, cycle, loop, child spawn/timeout, Unix secure log, logging; all §14.1–14.2 tests green on Linux; C++ child with CTest green. |
| M3 | Integration | Elevation, install/uninstall idempotency, `install.ps1`, README; the automated checklist items pass in the end-to-end CI job; the interactive items (UAC, standard-user denial, reboot) are executed on a VM. |
| M4 | Polish | CI workflow green on both runners; Clippy clean with warnings denied (`unwrap`/`expect` additionally denied outside tests; the pedantic group was considered and not adopted); `cargo doc` warnings clean; README verification section filled with real output. |

M1 exists because every unknown in this project is in the Windows security and SCM layers.
Discovering them on day one costs an afternoon; discovering them last costs the submission.

**Outcome.** The table above is the plan as written before implementation. In the event no
Windows VM was used: the end-to-end CI job on a hosted Windows runner (§14.5) executes the
checklist instead, including the standard-user denial and the UAC decline path that were
expected to need a human, and the two items a hosted runner cannot perform (the UAC Accept
click and an actual reboot) are listed in the README as not executed.

---

## 16. Risks and open items

| Risk | Mitigation |
|---|---|
| UAC relaunch argument quoting | rebuild argv with Windows quoting rules; covered by a unit test on the quoting helper |
| Stop not completing inside wait hint | cancellation checked inside child wait; child timeout 4 s < hint 10 s |
| `%ProgramData%` unset in an odd environment | fallback to `C:\ProgramData` |
| `windows-service` `Service::set_description` / `change_config` exact signatures | verify on docs.rs during planning; both are non-essential if absent (description optional, reinstall via uninstall+install) |
| AV / SmartScreen interference on the VM | documented; the checklist notes any prompt seen |
| Child killed mid-write leaves a partial line | acceptable at this scope; a single `write` of one short line is effectively atomic on both platforms |
| Registered binary path under `Program Files` contains a space | confirm the crate quotes `BINARY_PATH_NAME`; checklist item 2 shows the registered value verbatim, and item 12 (reboot) proves it launches |
| CMake picks no usable generator on the eval box | Build Tools 2022 ships CMake, Ninja, and the VS generator; the script fails fast with the tool name if `cmake` is absent |

---

## 17. README outline

The README is the reviewer's entry point; its job is to let someone reproduce the result
in ten minutes and see the evidence without reading code.

1. **What it is** — three sentences, plus the layout tree from §3, followed by an
   Architecture section with three Mermaid diagrams (components and trust boundary, one
   cycle as a sequence, the service lifecycle with failure paths) that GitHub renders inline.
2. **Confirmed interpretations** — the table from §1, so the reviewer sees the ambiguities
   were noticed and resolved with the author, not guessed.
3. **Prerequisites** — Rust stable (MSVC), VS Build Tools 2022 with C++ workload; nothing else.
4. **Build and install** — one command: `powershell -ExecutionPolicy Bypass -File .\install.ps1`.
   What it does, where files land, how to uninstall.
5. **Running interactively** — the bare invocation, the UAC prompt, Ctrl-C.
6. **Where the logs are and what a line looks like** — one real `agent.log` excerpt and one
   real `child.log` excerpt.
7. **Testing** — `cargo test`, `ctest`, the test tiers, coverage and mutation figures, and
   the Windows checklist from §14.4 with the end-to-end job's actual output pasted.
8. **Verified / not verified** — verbatim from §14.6, plus a statement of what CI proves and
   does not prove (§14.5). No badge: the repository is private, so a badge would not render
   for the reviewer.
9. **Design notes** — link to `docs/design.md` and its threat model, plus the seven points a
   reviewer most needs inline: protected DACL and why the directory is locked, why no
   elevation in service mode, the cycle isolation guarantees, static CRT, the platform
   boundary rule, child lifetime binding, and event log reporting.
10. **Limitations and next steps** — the non-goals from §1, and the systemd/launchd mapping.
