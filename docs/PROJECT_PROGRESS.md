# Project Progress

## Current snapshot

- Task: implement phase 1 from `CODEX_TASK_RU.md` on `fix/input-latency-kde-coordinates` at base `v1.1.0`.
- Status: phase 1 regression fix is committed in the current branch and all requested checks pass. Physical KDE/Waydroid validation and measured before/after latency remain outstanding.
- Worktree initially contained the user-provided untracked `CODEX_TASK_RU.md` and `TEST_MATRIX.md`; preserve both.
- Regression finding after `a83aee4`: `epoll_wait` uses a 10 ms timeout (not a zero-timeout busy loop). The daemon grabbed the keyboard for its entire lifetime and retransmitted every non-captured key through `/dev/uinput`; the main event loop also performs Android Socket writes synchronously. `InputCapture` is guarded by a mutex shared by the reader and IPC transitions. `SYN_DROPPED` resync handling already exists in `phantom/src/input.rs`.
- Comparison against `v1.1.0`: lifetime keyboard grab and uinput desktop relay were already present there, so source history does not support attributing that specific design to `a83aee4`. The new direct-KDE path removes it regardless. The reader still uses a bounded queue and can backpressure if an output stall persists long enough to fill both bounded queues; sustained backend stalls cannot be represented as zero latency without dropping/coalescing input.
- Implemented: dedicated `phantom-evdev-reader` thread polls, translates, and flushes input; bounded Tokio channel uses at most 128 batches of 64 events each, preserving order under backpressure. `InputCapture` is shared under one mutex, serializing reader state with IPC grab/hotplug controls.
- Implemented: opt-in `PHANTOM_INPUT_METRICS=1` measurements for evdev timestamp-to-routing p50/p95/p99, backend and relay write durations, queue depth/high-water, epoll drain gap, and `SYN_DROPPED` counts.
- Implemented: tests for 125/500/1000/8000 event streams, keyboard/mouse ordering, 200/500 ms bounded-queue stalls, reader shutdown, resync transitions and latency histogram percentiles.
- Validation so far: `cargo check --workspace --offline` passed; first `cargo test --workspace --offline` passed (110 library tests, 3 daemon tests, 29 integration tests; 4 ignored). A resync transition test was added after that run and still needs a final rerun.
- Commit: `a83aee4` (`perf: decouple evdev capture from backend writes`).
- Regression fix: physical keyboard is observed without `EVIOCGRAB` outside capture, so KDE receives ordinary keys directly. Keyboard grab follows capture state; capture transitions reconcile held keys and queued batches carry routing generations so stale pre-transition input is discarded after the engine's release/resync.
- Metrics now include evdev read time, queue wait percentiles, Android Socket send duration separately, process CPU percentage, and existing relay, route, queue depth, drain-gap, backend, and `SYN_DROPPED` measurements.
- Regression test simulates 200/500 ms output stalls and records ordered final sink delivery latency for interleaved keyboard/mouse streams. This is a deterministic mock queue test, not a live Android Socket or KDE measurement.
- Next action: run target-system acceptance and record physical before/after latency when KDE Wayland and Waydroid are available.
- Physical KDE/Waydroid latency measurements are not available from this environment and must not be claimed.
- Current follow-up: Stage A Android output decoupling is implemented in the worktree and passed the first offline clippy/test run. Stage B KDE KWin geometry integration and its commit are still pending.

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
- Commit created: `a83aee4` (`perf: decouple evdev capture from backend writes`).
- Next action: run target-system acceptance tests when KDE Wayland and Waydroid are available; capture before/after latency percentiles and confirm desktop keyboard relay behavior.

### 2026-11-11 — phase 1 KDE regression fix

- Rechecked reader timeout and event routing: epoll wait is 10 ms, while synchronous Android Socket application remains on the main loop. The reported idle typing regression had a direct architectural cause: keyboard EVIOCGRAB remained enabled outside capture and each key was retransmitted through uinput.
- Changed startup to observe keyboard events without grabbing. Outside capture, ordinary physical key events are no longer relayed through uinput. Keyboard grabs are now enabled only during capture, and held-key state is reconciled on both transitions. Desktop relay only releases keys Phantom seeded on capture exit.
- Added a routing generation to batches, dropping queued events from the previous ownership mode after a capture transition.
- Added opt-in measurements for evdev reads, batch queue wait, Android Socket duration/stalls, and process CPU, retaining routing, relay, queue depth/high-water, max epoll drain gap, and `SYN_DROPPED` counts.
- Added a synthetic final-sink delivery test with interleaved key down/mouse movement/key up under 200 ms and 500 ms modeled output stalls; it checks ordered delivery and end latency. It does not use a live Android Socket.
- Final `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `cargo build --release --workspace` passed after the final transition-order/metric-count adjustment. `git diff --check` passed.
- Physical KDE+Waydroid typing latency and a live installed Phantom 1.1.0 comparison are not available in this environment.
- Source-only comparison against `v1.1.0` confirms its startup also grabs the keyboard for runtime hotkeys and relays non-capture keyboard events through uinput. No live comparison against the installed package was performed.
- Phase 1 regression fix committed to `fix/input-latency-kde-coordinates`; target-system validation remains the next action.

### 2026-11-11 — active capture output worker (Stage A)

- Reproduced the blocking architecture by tracing engine output: `main.rs` synchronously called `TouchDevice::apply_commands`, which writes/flushes each Android Socket protocol frame before the event loop can route more input.
- Added a dedicated `phantom-android-output` worker with one bounded FIFO queue for all touch and Android key commands. All regular event routing enqueues without waiting for socket completion; IPC/capture mode transitions use a synchronous completion barrier on the same queue to preserve order.
- On backend errors, the worker makes a best-effort release of tracked Android keys and calls `release_all()` for active contacts. Bounded queue fullness applies backpressure without dropping transitions.
- Added physical evdev timestamp-to-send, output queue wait/depth/high-water, backend duration, and Android Socket duration measurements. Updated architecture and operations documentation.
- Added deterministic worker delivery tests for 100/200/500 ms backend delays with interleaved touch and keyboard commands, and daemon pipeline tests for 200/500 ms stalls and bounded queue backpressure. Assertions observe final mock sink delivery and exact command order, not only reader speed.
- `cargo fmt --all`, `cargo clippy --workspace --all-targets --offline -- -D warnings`, and `cargo test --workspace --offline` passed. Counts: 113 library tests, 4 daemon tests, 29 integration tests passed; 4 ignored.
- Physical F8/F1/WASD/mouse acceptance and live Android Socket latency are not measured here. The tests use deterministic mock backends. Stage A still requires release build and final checks before commit.
