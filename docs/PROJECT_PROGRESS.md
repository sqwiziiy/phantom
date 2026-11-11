# Project Progress

## Current snapshot

- Task: implement phase 1 from `CODEX_TASK_RU.md` on `fix/input-latency-kde-coordinates` at base `v1.1.0`.
- Status: phase 1 implementation and automated verification are complete. Physical KDE/Waydroid validation and measured before/after latency remain outstanding.
- Worktree initially contained the user-provided untracked `CODEX_TASK_RU.md` and `TEST_MATRIX.md`; preserve both.
- Initial runtime finding: the Tokio daemon loop polls and routes evdev events and performs synchronous backend writes. `InputCapture` is guarded by a mutex shared by the loop and IPC transitions. `SYN_DROPPED` resync handling already exists in `phantom/src/input.rs`.
- Implemented: dedicated `phantom-evdev-reader` thread polls, translates, and flushes input; bounded Tokio channel uses at most 128 batches of 64 events each, preserving order under backpressure. `InputCapture` is shared under one mutex, serializing reader state with IPC grab/hotplug controls.
- Implemented: opt-in `PHANTOM_INPUT_METRICS=1` measurements for evdev timestamp-to-routing p50/p95/p99, backend and relay write durations, queue depth/high-water, epoll drain gap, and `SYN_DROPPED` counts.
- Implemented: tests for 125/500/1000/8000 event streams, keyboard/mouse ordering, 200/500 ms bounded-queue stalls, reader shutdown, resync transitions and latency histogram percentiles.
- Validation so far: `cargo check --workspace --offline` passed; first `cargo test --workspace --offline` passed (110 library tests, 3 daemon tests, 29 integration tests; 4 ignored). A resync transition test was added after that run and still needs a final rerun.
- Next action: inspect the final diff, record the exact changed files, and commit the phase 1 implementation.
- Physical KDE/Waydroid latency measurements are not available from this environment and must not be claimed.

## Evidence log

### 2026-11-11 — phase 1 investigation

- Verified branch `fix/input-latency-kde-coordinates`, HEAD `75624c3` tagged `v1.1.0`.
- Read task brief, repository contribution instructions, README, architecture/profile docs, testing and operations docs, and inspected `input.rs`, `main.rs`, `ipc.rs`, and `desktop_relay.rs`.
- No implementation or automated test has run yet.

### 2026-11-11 — phase 1 implementation and first test run

- Changed `phantom/src/main.rs` and `phantom/src/ipc.rs` to run evdev poll/translation on a dedicated thread and feed ordered bounded batches to the daemon loop. Reader shutdown is tied to daemon shutdown and receiver closure.
- Added `phantom/src/input_metrics.rs`; documented metrics in README and operations guide; factored key-state reconciliation for a deterministic lost-key resync test.
- First `cargo test --workspace --offline`: passed 110 library tests, 3 daemon tests, and 29 integration tests; 4 environment-dependent tests ignored. The separate lost-key reconciliation test was added afterward and is pending the final run.
- A follow-up verification caught an associated-function call missing `Self::` in the new key reconciliation helper. Corrected it; rerunning verification.
- Final test run passed 111 library tests, 3 daemon pipeline tests, and 29 integration tests; 4 environment-dependent tests were ignored. Clippy identified a manual division guard in the new metric average helper; replaced it with checked division before rerunning clippy/build.
- Real KDE/Waydroid before/after latency and physical keyboard behavior remain unmeasured in this environment.

### 2026-11-11 — final phase 1 verification

- `cargo fmt --all -- --check`: passed.
- `cargo test --workspace --offline`: passed; 111 library tests, 3 daemon pipeline tests, and 29 integration tests passed, with 4 environment-dependent tests ignored.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: passed.
- `cargo build --release --workspace --offline`: passed.
- `git diff --check`: passed.
- The 200/500 ms tests simulate a stalled bounded-channel consumer; they do not exercise a live Android TCP socket. No physical KDE/Waydroid performance run was available, so no before/after latency claim is made.
- Remaining limitation: capture controls and hotplug share the capture mutex with the reader; they serialize safely but are not routed through a dedicated control-message owner. Physical typing and Waydroid behavior still need acceptance testing on the target system.
- Next action: inspect the final diff, record the exact changed files, and commit the phase 1 implementation.
