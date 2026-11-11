use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing_subscriber::EnvFilter;

use phantom::config;
use phantom::desktop_relay::DesktopKeyboardRelay;
use phantom::engine::KeymapEngine;
use phantom::error::{PhantomError, Result};
use phantom::input::{InputCapture, InputEvent};
use phantom::input_metrics::InputMetrics;
use phantom::ipc::{self, DaemonState, IpcRequest, MouseMode};
use phantom::logging::trace_detail_enabled;
use phantom::profile::Profile;
use phantom::touch;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const ENGINE_TICK_INTERVAL_MS: u64 = 4;
const INPUT_QUEUE_BATCHES: usize = 128;
const INPUT_BATCH_EVENTS: usize = 64;

struct InputBatch {
    events: Vec<InputEvent>,
    event_timestamps: Vec<(i64, i64)>,
    syn_dropped_count: usize,
    queued_at: std::time::Instant,
    routing_generation: u64,
}

fn print_help() {
    println!(
        r#"{app_name} {app_version}

Virtual touchscreen for Waydroid

USAGE:
    {app_name} --daemon                  Start the daemon
    {app_name} version                   Show version
    {app_name} audit <profile.json>      Audit slot and binding usage in a profile
    {app_name} load <profile.json>       Load a profile
    {app_name} status                    Show daemon status
    {app_name} pause                     Pause input processing
    {app_name} resume                    Resume input processing
    {app_name} reload                    Reload current profile
    {app_name} enter-capture             Enable exclusive gameplay capture
    {app_name} exit-capture              Release exclusive gameplay capture
    {app_name} toggle-capture            Toggle gameplay capture
    {app_name} grab-mouse                Switch owned mouse to gameplay aim
    {app_name} release-mouse             Switch owned mouse to menu touch
    {app_name} toggle-mouse              Toggle owned mouse between aim and menu touch
    {app_name} sensitivity <value>       Set global sensitivity
    {app_name} list                      List available profiles
    {app_name} shutdown                  Graceful shutdown

KEYS (while daemon running, configurable in config.toml [runtime_hotkeys]):
    F2   Shutdown daemon (default)
    F1   Toggle aim / menu-touch (default)
    F8   Toggle capture mode (default)
    F9   Toggle pause (default)
    F10  Toggle experimental debug control preview (default)

FLAGS:
    --daemon      Run as daemon (requires root)
    --trace       Force trace logging for this run
                  Use PHANTOM_TRACE_DETAIL=1 for raw-device trace detail
    -h, --help    Show this help
    -V, --version Show version"#,
        app_name = APP_NAME,
        app_version = APP_VERSION,
    );
}

fn print_version() {
    println!("{} {}", APP_NAME, APP_VERSION);
}

fn expand_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).display().to_string();
        }
    }
    path.to_string()
}

fn format_slots(slots: &[u8]) -> String {
    let mut slots = slots.to_vec();
    slots.sort_unstable();
    slots.dedup();
    if slots.is_empty() {
        "(none)".into()
    } else {
        slots
            .into_iter()
            .map(|slot| slot.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn format_bindings(bindings: &[String]) -> String {
    if bindings.is_empty() {
        "(none)".into()
    } else {
        bindings.join(", ")
    }
}

fn print_profile_audit(profile: &Profile) {
    let audit = profile.audit();
    let slots: Vec<u8> = audit.touch_entries.iter().map(|entry| entry.slot).collect();

    eprintln!("profile: {}", audit.profile_name);
    eprintln!("screen: {}x{}", audit.screen_width, audit.screen_height);
    eprintln!("total nodes: {}", audit.total_nodes);
    eprintln!("touch nodes: {}", audit.touch_entries.len());
    eprintln!("touch slots: {}", format_slots(&slots));

    if !audit.touch_entries.is_empty() {
        eprintln!("touch slot audit:");
        for entry in &audit.touch_entries {
            let mut line = format!(
                "  slot {}  {}  id={} layer={} bindings={}",
                entry.slot,
                entry.node_type,
                entry.node_id,
                entry.layer,
                format_bindings(&entry.bindings)
            );
            if let Some(detail) = &entry.detail {
                line.push_str(&format!(" {}", detail));
            }
            eprintln!("{line}");
        }
    }

    if !audit.auxiliary_entries.is_empty() {
        eprintln!("auxiliary nodes:");
        for entry in &audit.auxiliary_entries {
            let mut line = format!(
                "  {}  id={} layer={} bindings={}",
                entry.node_type,
                entry.node_id,
                entry.layer,
                format_bindings(&entry.bindings)
            );
            if let Some(detail) = &entry.detail {
                line.push_str(&format!(" {}", detail));
            }
            eprintln!("{line}");
        }
    }
}

fn init_logging(force_trace: bool) {
    let default_level = if force_trace
        || matches!(
            std::env::var("PHANTOM_TRACE").ok().as_deref(),
            Some("1" | "true" | "TRUE" | "yes" | "YES")
        ) {
        "trace".to_string()
    } else {
        let cfg = config::load_config();
        if cfg.log_level.trim().is_empty() {
            "info".to_string()
        } else {
            cfg.log_level
        }
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .init();
}

fn detect_resolution(config: &config::Config, profile: Option<&Profile>) -> Result<(u32, u32)> {
    match (config.screen.width, config.screen.height) {
        (Some(w), Some(h)) => {
            tracing::info!("screen resolution from config: {}x{}", w, h);
            Ok((w, h))
        }
        (Some(_), None) | (None, Some(_)) => Err(PhantomError::ResolutionDetection(
            "config screen requires both width and height".into(),
        )),
        (None, None) => {
            if let Some(screen) = profile.and_then(|p| p.screen.as_ref()) {
                tracing::info!(
                    "screen resolution from default profile '{}': {}x{}",
                    profile.map(|p| p.name.as_str()).unwrap_or("unknown"),
                    screen.width,
                    screen.height
                );
                Ok((screen.width, screen.height))
            } else {
                Err(PhantomError::ResolutionDetection(
                    "fullscreen mode requires an explicit resolution in config.toml or the default profile".into(),
                ))
            }
        }
    }
}

async fn run_daemon() -> Result<()> {
    tracing::info!("phantom {} starting", env!("CARGO_PKG_VERSION"));

    let config = config::load_config();
    let default_profile_path = config::default_profile_path();
    let default_profile = default_profile_path.as_ref().and_then(|path| {
        tracing::info!("loading default profile: {}", path.display());
        match Profile::load(path) {
            Ok(profile) => Some(profile),
            Err(e) => {
                tracing::warn!("failed to load default profile {}: {}", path.display(), e);
                None
            }
        }
    });

    let (screen_w, screen_h) = detect_resolution(&config, default_profile.as_ref())?;
    let runtime_hotkeys = config::resolved_runtime_hotkeys(&config);
    let touch = touch::create_touch_device(&config, screen_w, screen_h)?;
    let desktop_keyboard = DesktopKeyboardRelay::new()?;
    let mut capture = InputCapture::discover()?;
    // Keep the physical keyboard on KDE's direct path while capture is off.
    // The reader still observes evdev events for global runtime hotkeys.
    capture.set_grabbed_keyboard_only(false)?;
    tracing::info!("keyboard observation enabled without exclusive grab");
    tracing::info!(
        devices = capture.device_count(),
        mouse = capture.has_mouse(),
        keyboard = capture.has_keyboard(),
        "input capture ready"
    );

    let engine = match default_profile {
        Some(p) => KeymapEngine::new(p),
        None => {
            tracing::warn!("no default profile found, engine idle until profile loaded via IPC");
            KeymapEngine::new(phantom::profile::Profile {
                name: "empty".into(),
                version: 1,
                screen: Some(phantom::profile::ScreenOverride {
                    width: screen_w,
                    height: screen_h,
                }),
                global_sensitivity: 1.0,
                nodes: vec![],
            })
        }
    };

    let (state, mut shutdown_rx) =
        DaemonState::new(engine, touch, desktop_keyboard, capture, screen_w, screen_h);
    let input_metrics = Arc::new(InputMetrics::from_environment());
    if input_metrics.enabled() {
        tracing::info!("input metrics enabled by PHANTOM_INPUT_METRICS");
    }
    {
        let mut mouse_touch = ipc::lock_mouse_touch(&state)?;
        mouse_touch.apply_feel(
            phantom::mouse_touch::MouseTouchAccelConfig::from_config(&config.mouse_touch),
            config.mouse_touch.scroll_step,
        );
    }
    if let Some(path) = default_profile_path {
        *state.profile_path.write().await = Some(path);
    }
    let state_clone = state.clone();

    let _ipc_handle = tokio::spawn(async move {
        if let Err(e) = ipc::run_ipc_server(state_clone).await {
            tracing::error!("IPC server error: {}", e);
        }
    });

    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<InputBatch>(INPUT_QUEUE_BATCHES);
    let input_worker_stop = Arc::new(AtomicBool::new(false));
    let worker_stop = input_worker_stop.clone();
    let worker_state = state.clone();
    let worker_metrics = input_metrics.clone();
    let input_worker = std::thread::Builder::new()
        .name("phantom-evdev-reader".into())
        .spawn(move || {
            let mut last_drain = std::time::Instant::now();
            while !worker_stop.load(Ordering::Acquire) {
                let read_started = std::time::Instant::now();
                let events = match ipc::lock_capture(&worker_state) {
                    Ok(mut capture) => {
                        let routing_generation =
                            worker_state.routing_generation.load(Ordering::Acquire);
                        match capture.poll_events(10) {
                            Ok(raw_events) => {
                                let event_timestamps = raw_events
                                    .iter()
                                    .map(|(_, event)| (event.tv_sec, event.tv_usec))
                                    .collect();
                                let syn_dropped_count = raw_events
                                    .iter()
                                    .filter(|(_, event)| event.type_ == 0 && event.code == 3)
                                    .count();
                                let mut events = if raw_events.is_empty() {
                                    Vec::new()
                                } else {
                                    capture.process_events(&raw_events)
                                };
                                events.extend(capture.flush_pending_touchpad_taps());
                                Ok((
                                    events,
                                    event_timestamps,
                                    syn_dropped_count,
                                    routing_generation,
                                ))
                            }
                            Err(err) => Err(err),
                        }
                    }
                    Err(err) => {
                        tracing::warn!("input reader lock error: {}", err);
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                };
                worker_metrics.record_evdev_read(read_started.elapsed());
                worker_metrics.record_drain_gap(last_drain.elapsed());
                last_drain = std::time::Instant::now();
                let (events, event_timestamps, syn_dropped_count, routing_generation) = match events
                {
                    Ok(events) => events,
                    Err(err) => {
                        tracing::warn!("input reader poll error: {}", err);
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                };
                if events.is_empty() && syn_dropped_count == 0 {
                    continue;
                }
                if events.is_empty() {
                    if input_tx
                        .blocking_send(InputBatch {
                            events,
                            event_timestamps,
                            syn_dropped_count,
                            queued_at: std::time::Instant::now(),
                            routing_generation,
                        })
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                let chunk_count = events.len().div_ceil(INPUT_BATCH_EVENTS);
                let mut event_chunks = events.chunks(INPUT_BATCH_EVENTS);
                let timestamp_chunk_size = event_timestamps.len().div_ceil(chunk_count).max(1);
                let mut timestamp_chunks = event_timestamps.chunks(timestamp_chunk_size);
                for chunk_index in 0..chunk_count {
                    let chunk = event_chunks.next().expect("chunk count matches events");
                    let timestamps = timestamp_chunks
                        .next()
                        .map_or_else(Vec::new, |chunk| chunk.to_vec());
                    worker_metrics.queue_add(chunk.len());
                    let batch = InputBatch {
                        events: chunk.to_vec(),
                        event_timestamps: timestamps,
                        syn_dropped_count: if chunk_index == 0 {
                            syn_dropped_count
                        } else {
                            0
                        },
                        queued_at: std::time::Instant::now(),
                        routing_generation,
                    };
                    if input_tx.blocking_send(batch).is_err() {
                        worker_metrics.queue_remove(chunk.len());
                        break;
                    }
                }
            }
        })?;

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let flag = shutdown_flag.clone();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM");
            }
            _ = sigint.recv() => {
                tracing::info!("received SIGINT");
            }
        }
        flag.store(true, Ordering::Release);
    });

    tracing::info!(
        mouse_toggle = ?runtime_hotkeys.mouse_toggle,
        capture_toggle = ?runtime_hotkeys.capture_toggle,
        pause_toggle = ?runtime_hotkeys.pause_toggle,
        overlay_toggle = ?runtime_hotkeys.overlay_toggle,
        shutdown = ?runtime_hotkeys.shutdown,
        "daemon ready, entering event loop"
    );

    // 4 ms is a better compromise for repeat-tap, drag, and macro timing than
    // the old frame-like 16 ms cadence, while still avoiding a 1 ms hot loop.
    let mut tick_interval = tokio::time::interval(Duration::from_millis(ENGINE_TICK_INTERVAL_MS));
    tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_frame_refresh = std::time::Instant::now();
    let mut last_input_rescan = std::time::Instant::now();

    loop {
        if shutdown_flag.load(Ordering::Acquire) {
            tracing::info!("shutdown signal received");
            break;
        }

        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::info!("IPC shutdown command received");
                break;
            }
            input_batch = input_rx.recv() => {
                let Some(input_batch) = input_batch else {
                    tracing::error!("input reader channel closed unexpectedly");
                    break;
                };
                input_metrics.queue_remove(input_batch.events.len());
                input_metrics.record_queue_wait(
                    input_batch.queued_at.elapsed(),
                    input_batch.events.len(),
                );
                for timestamp in &input_batch.event_timestamps {
                    input_metrics.record_evdev_to_route_latency(*timestamp);
                }
                input_metrics.record_syn_dropped(input_batch.syn_dropped_count);
                let input_events = input_batch.events;
                if input_events.is_empty() {
                    continue;
                }
                let batch_generation = input_batch.routing_generation;
                if state.routing_generation.load(Ordering::Acquire) != batch_generation {
                    continue;
                }
                tracing::trace!(events = ?input_events, "translated input batch");

                        let mut gameplay_events = Vec::new();
                        let (mut mouse_mode, mut keyboard_routed) = match ipc::lock_capture(&state) {
                            Ok(capture) => {
                                let keyboard_routed = capture.keyboard_grabbed();
                                drop(capture);
                                match ipc::current_mouse_mode(&state) {
                                    Ok(mouse_mode) => (mouse_mode, keyboard_routed),
                                    Err(e) => {
                                        tracing::warn!("mouse mode error: {}", e);
                                        continue;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("input capture lock error: {}", e);
                                continue;
                            }
                        };
                        for event in input_events {
                            match handle_runtime_shortcut(&state, &event, &runtime_hotkeys).await {
                                Ok(true) => {
                                    match ipc::lock_capture(&state) {
                                        Ok(capture) => {
                                            keyboard_routed = capture.keyboard_grabbed();
                                        }
                                        Err(e) => {
                                            tracing::warn!("input capture lock error: {}", e);
                                        }
                                    }
                                    match ipc::current_mouse_mode(&state) {
                                        Ok(mode) => {
                                            mouse_mode = mode;
                                        }
                                        Err(e) => {
                                            tracing::warn!("mouse mode error: {}", e);
                                        }
                                    }
                                    continue;
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    tracing::warn!("runtime shortcut error: {}", e);
                                    continue;
                                }
                            }
                            if state.capture_active.load(Ordering::Acquire) {
                                if event.is_mouse_input() && matches!(mouse_mode, MouseMode::MenuTouch) {
                                    let mouse_touch_cmds = match ipc::process_menu_touch_event(&state, &event) {
                                        Ok(cmds) => cmds,
                                        Err(e) => {
                                            tracing::warn!("mouse-touch processing error: {}", e);
                                            Vec::new()
                                        }
                                    };
                                    let backend_started = std::time::Instant::now();
                                    let backend_result = if mouse_touch_cmds.is_empty() {
                                        Ok(())
                                    } else {
                                        let mut dev = match ipc::lock_touch_device(&state) {
                                            Ok(dev) => dev,
                                            Err(e) => {
                                                tracing::warn!("touch backend lock error: {}", e);
                                                continue;
                                            }
                                        };
                                        let is_android_socket = dev.backend_name() == "android_socket";
                                        let result = dev.apply_commands(&mouse_touch_cmds);
                                        if is_android_socket {
                                            input_metrics.record_android_socket(backend_started.elapsed());
                                        }
                                        result
                                    };
                                    input_metrics.record_backend(backend_started.elapsed());
                                    if let Err(e) = backend_result {
                                        tracing::warn!("mouse-touch inject error: {}", e);
                                    }
                                    if trace_detail_enabled() && !mouse_touch_cmds.is_empty() {
                                        tracing::trace!(
                                            event = ?event,
                                            commands = ?mouse_touch_cmds,
                                            "translated mouse input into menu touch"
                                        );
                                    }
                                    continue;
                                }
                                if event.is_keyboard_input() && !keyboard_routed {
                                    if trace_detail_enabled() {
                                        tracing::trace!(
                                            event = ?event,
                                            "dropping gameplay event because keyboard routing is disabled"
                                        );
                                    }
                                    continue;
                                }
                                gameplay_events.push(event);
                            } else {
                                if let InputEvent::KeyRelease(key) = &event {
                                    let relay_started = std::time::Instant::now();
                                    let relay_result = ipc::lock_desktop_keyboard(&state)
                                        .and_then(|mut relay| relay.release_if_pressed(*key));
                                    if let Err(e) = relay_result {
                                        tracing::warn!("desktop keyboard relay error: {}", e);
                                    }
                                    input_metrics.record_relay(relay_started.elapsed());
                                }
                                if trace_detail_enabled() {
                                    tracing::trace!(
                                        event = ?event,
                                        "dropping gameplay event because capture is inactive"
                                    );
                                }
                            }
                        }

                        if state.routing_generation.load(Ordering::Acquire) != batch_generation {
                            continue;
                        }
                        if !gameplay_events.is_empty() {
                            tracing::trace!(events = ?gameplay_events, "forwarding gameplay events to engine");
                            let mut engine = state.engine.write().await;
                            let mut dev = match ipc::lock_touch_device(&state) {
                                Ok(dev) => dev,
                                Err(e) => {
                                    tracing::warn!("touch backend lock error: {}", e);
                                    continue;
                                }
                            };
                            for event in &gameplay_events {
                                if state.routing_generation.load(Ordering::Acquire) != batch_generation {
                                    break;
                                }
                                let cmds = engine.process(event);
                                let cmds = if cmds.is_empty() {
                                    match ipc::process_passthrough_event(&state, event, &engine) {
                                        Ok(passthrough) => passthrough,
                                        Err(e) => {
                                            tracing::warn!("passthrough error: {}", e);
                                            Vec::new()
                                        }
                                    }
                                } else {
                                    cmds
                                };
                                if cmds.is_empty() {
                                    continue;
                                }
                                let backend_started = std::time::Instant::now();
                                let is_android_socket = dev.backend_name() == "android_socket";
                                let backend_result = dev.apply_commands(&cmds);
                                let elapsed = backend_started.elapsed();
                                input_metrics.record_backend(elapsed);
                                if is_android_socket {
                                    input_metrics.record_android_socket(elapsed);
                                }
                                if let Err(e) = backend_result {
                                    tracing::warn!("inject error: {}", e);
                                    break;
                                }
                            }
                        }
            }
            _ = tick_interval.tick() => {
                input_metrics.maybe_report();
                if last_input_rescan.elapsed() >= Duration::from_secs(2) {
                    last_input_rescan = std::time::Instant::now();
                    if let Ok(mut capture) = ipc::lock_capture(&state) {
                        if let Err(err) = capture.rescan_devices() {
                            tracing::warn!("input hotplug rescan failed: {}", err);
                        }
                    }
                }
                if !state.capture_active.load(Ordering::Acquire) {
                    continue;
                }
                if last_frame_refresh.elapsed() >= Duration::from_millis(250)
                    && matches!(ipc::current_mouse_mode(&state), Ok(MouseMode::MenuTouch))
                {
                    last_frame_refresh = std::time::Instant::now();
                    if let Ok(mut mouse_touch) = ipc::lock_mouse_touch(&state) {
                        if mouse_touch.refresh_host_frame() {
                            drop(mouse_touch);
                            let _ = ipc::sync_cursor_overlay(&state, true);
                        }
                    }
                }
                let mut engine = state.engine.write().await;
                let mut cmds = engine.tick();
                drop(engine);
                match ipc::tick_passthrough_keys(&state) {
                    Ok(passthrough) => cmds.extend(passthrough),
                    Err(e) => tracing::warn!("passthrough tick error: {}", e),
                }
                if !cmds.is_empty() {
                    let mut dev = match ipc::lock_touch_device(&state) {
                        Ok(dev) => dev,
                        Err(e) => {
                            tracing::warn!("touch backend lock error: {}", e);
                            continue;
                        }
                    };
                    let backend_started = std::time::Instant::now();
                    let is_android_socket = dev.backend_name() == "android_socket";
                    let backend_result = dev.apply_commands(&cmds);
                    let elapsed = backend_started.elapsed();
                    input_metrics.record_backend(elapsed);
                    if is_android_socket {
                        input_metrics.record_android_socket(elapsed);
                    }
                    if let Err(e) = backend_result {
                        tracing::warn!("inject error: {}", e);
                    }
                }
            }
        }
    }

    tracing::info!("performing clean shutdown...");

    input_worker_stop.store(true, Ordering::Release);
    drop(input_rx);
    if let Err(err) = input_worker.join() {
        tracing::warn!("input reader thread did not stop cleanly: {:?}", err);
    }

    {
        let mut engine = state.engine.write().await;
        let cmds = engine.release_all();
        drop(engine);
        let mut dev = ipc::lock_touch_device(&state)?;
        let _ = dev.apply_commands(&cmds);
        if let Ok(passthrough) = ipc::release_passthrough_keys(&state) {
            let _ = dev.apply_commands(&passthrough);
        }
    }

    {
        if let Ok(mut relay) = ipc::lock_desktop_keyboard(&state) {
            let _ = relay.release_all();
        }
    }

    {
        if let Ok(mut overlay) = ipc::lock_overlay(&state) {
            let _ = overlay.stop();
        }
    }

    {
        let _ = ipc::stop_cursor_overlay(&state);
    }

    {
        if let Ok(mut mouse_touch) = ipc::lock_mouse_touch(&state) {
            let cmds = mouse_touch.suspend();
            if !cmds.is_empty() {
                if let Ok(mut dev) = ipc::lock_touch_device(&state) {
                    let _ = dev.apply_commands(&cmds);
                }
            }
        }
    }

    {
        let mut capture = ipc::lock_capture(&state)?;
        capture.force_release_all();
    }

    let socket_path = config::socket_path();
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    tracing::info!("phantom shutdown complete");
    Ok(())
}

async fn handle_runtime_shortcut(
    state: &Arc<DaemonState>,
    event: &InputEvent,
    hotkeys: &config::RuntimeHotkeys,
) -> Result<bool> {
    if let InputEvent::KeyRelease(key) = event {
        if is_runtime_hotkey(*key, hotkeys) {
            return Ok(true);
        }
    }

    match event {
        InputEvent::KeyPress(key) if hotkeys.capture_toggle == Some(*key) => {
            tracing::info!(?key, "runtime hotkey pressed");
            let next = !state.capture_active.load(Ordering::Acquire);
            ipc::set_capture_active(state, next).await?;
            tracing::info!("capture {}", if next { "enabled" } else { "disabled" });
            Ok(true)
        }
        InputEvent::KeyPress(key) if hotkeys.pause_toggle == Some(*key) => {
            tracing::info!(?key, "runtime hotkey pressed");
            let cmds = {
                let mut engine = state.engine.write().await;
                if engine.is_paused() {
                    engine.resume();
                    tracing::info!("input processing resumed");
                    Vec::new()
                } else {
                    tracing::info!("input processing paused");
                    engine.pause()
                }
            };
            if !cmds.is_empty() {
                let mut dev = ipc::lock_touch_device(state)?;
                let _ = dev.apply_commands(&cmds);
            }
            Ok(true)
        }
        InputEvent::KeyPress(key) if hotkeys.overlay_toggle == Some(*key) => {
            tracing::info!(?key, "runtime hotkey pressed");
            let profile = {
                let engine = state.engine.read().await;
                engine.profile_clone()
            };
            let preview_frame = {
                let mut mouse_touch = ipc::lock_mouse_touch(state)?;
                if mouse_touch.overlay_frame().is_none() {
                    mouse_touch.seed_from_host_cursor();
                }
                mouse_touch.overlay_frame()
            };
            let visible = {
                let mut overlay = ipc::lock_overlay(state)?;
                overlay.toggle(&profile, preview_frame)?
            };
            tracing::info!(
                "{}",
                if visible {
                    "experimental overlay preview enabled"
                } else {
                    "experimental overlay preview disabled"
                }
            );
            Ok(true)
        }
        InputEvent::KeyPress(key) if hotkeys.mouse_toggle == Some(*key) => {
            tracing::info!(?key, "runtime hotkey pressed");
            if !state.capture_active.load(Ordering::Acquire) {
                tracing::info!("mouse toggle ignored (not in capture mode)");
                return Ok(true);
            }
            let current_mode = ipc::current_mouse_mode(state)?;
            let next_is_aim = matches!(current_mode, MouseMode::MenuTouch);
            ipc::set_mouse_routed(state, next_is_aim).await?;
            tracing::info!(
                "{}",
                if next_is_aim {
                    "mouse switched to gameplay aim"
                } else {
                    "mouse switched to owned menu-touch navigation"
                }
            );
            Ok(true)
        }
        InputEvent::KeyPress(key) if hotkeys.shutdown == Some(*key) => {
            tracing::info!(?key, "runtime hotkey pressed");
            // Signal shutdown — cleanup happens in the main loop
            state.shutdown_tx.send(()).ok();
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn is_runtime_hotkey(key: phantom::input::Key, hotkeys: &config::RuntimeHotkeys) -> bool {
    hotkeys.mouse_toggle == Some(key)
        || hotkeys.capture_toggle == Some(key)
        || hotkeys.pause_toggle == Some(key)
        || hotkeys.overlay_toggle == Some(key)
        || hotkeys.shutdown == Some(key)
}

async fn run_cli_command(args: &[String]) -> Result<()> {
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("status");

    match cmd {
        "version" => {
            print_version();
            return Ok(());
        }
        "audit" => {
            let path = args.get(1).ok_or_else(|| {
                phantom::error::PhantomError::Ipc("audit requires a path argument".into())
            })?;
            let path = expand_path(path);
            let profile = Profile::load(std::path::Path::new(&path))?;
            print_profile_audit(&profile);
            return Ok(());
        }
        _ => {}
    }

    let request = match cmd {
        "load" => {
            let path = args.get(1).ok_or_else(|| {
                phantom::error::PhantomError::Ipc("load requires a path argument".into())
            })?;
            IpcRequest::LoadProfile { path: path.clone() }
        }
        "status" => IpcRequest::Status,
        "pause" => IpcRequest::Pause,
        "resume" => IpcRequest::Resume,
        "reload" => IpcRequest::Reload,
        "enter-capture" => IpcRequest::EnterCapture,
        "exit-capture" => IpcRequest::ExitCapture,
        "toggle-capture" => IpcRequest::ToggleCapture,
        "grab-mouse" => IpcRequest::GrabMouse,
        "release-mouse" => IpcRequest::ReleaseMouse,
        "toggle-mouse" => IpcRequest::ToggleMouse,
        "shutdown" => IpcRequest::Shutdown,
        "list" => IpcRequest::ListProfiles,
        "sensitivity" => {
            let value = args
                .get(1)
                .ok_or_else(|| {
                    phantom::error::PhantomError::Ipc("sensitivity requires a value".into())
                })?
                .parse::<f64>()
                .map_err(|_| {
                    phantom::error::PhantomError::Ipc("invalid sensitivity value".into())
                })?;
            IpcRequest::SetSensitivity { value }
        }
        other => {
            eprintln!("unknown command: {}", other);
            eprintln!("run 'phantom --help' for usage");
            std::process::exit(1);
        }
    };

    let response = ipc::send_command(&request).await?;

    if response.ok {
        if let Some(msg) = &response.message {
            eprintln!("{}", msg);
        }
        if let Some(profile) = &response.profile {
            eprintln!("profile: {}", profile);
        }
        if let Some(path) = &response.profile_path {
            eprintln!("profile path: {}", path);
        }
        if let (Some(w), Some(h)) = (response.screen_width, response.screen_height) {
            eprintln!("screen: {}x{}", w, h);
        }
        if let Some(nodes) = response.nodes {
            eprintln!("nodes: {}", nodes);
        }
        if let Some(slots) = &response.slots {
            eprintln!("slots: {}", format_slots(slots));
        }
        if let Some(paused) = response.paused {
            eprintln!("paused: {}", paused);
        }
        if let Some(capture_active) = response.capture_active {
            eprintln!("capture: {}", capture_active);
        }
        if let Some(mouse_grabbed) = response.mouse_grabbed {
            eprintln!("mouse grabbed: {}", mouse_grabbed);
        }
        if let Some(mouse_mode) = &response.mouse_mode {
            eprintln!("mouse mode: {}", mouse_mode);
        }
        if let Some(mouse_touch_active) = response.mouse_touch_active {
            eprintln!("menu touch: {}", mouse_touch_active);
        }
        if let Some(mouse_touch_backend) = &response.mouse_touch_backend {
            eprintln!("menu touch backend: {}", mouse_touch_backend);
        }
        if let Some(keyboard_grabbed) = response.keyboard_grabbed {
            eprintln!("keyboard grabbed: {}", keyboard_grabbed);
        }
        if let Some(sensitivity) = response.sensitivity {
            eprintln!("sensitivity: {}", sensitivity);
        }
        if let Some(active_layers) = &response.active_layers {
            if active_layers.is_empty() {
                eprintln!("active layers: (none)");
            } else {
                eprintln!("active layers: {}", active_layers.join(", "));
            }
        }
        if let Some(profiles) = &response.profiles {
            if profiles.is_empty() {
                eprintln!("no profiles found");
            } else {
                for p in profiles {
                    eprintln!("  {} — {}", p.name, p.path);
                }
            }
        }
    } else {
        if let Some(err) = &response.error {
            eprintln!("error: {}", err);
        }
        std::process::exit(1);
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s.as_str()) == Some("__hyprland_cursor_helper") {
        if let Err(e) = phantom::hyprland_cursor::run_helper_stdio() {
            eprintln!("hyprland cursor helper error: {}", e);
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|s| s.as_str()) == Some("__x11_cursor_helper") {
        if let Err(e) = phantom::x11_cursor::run_helper_stdio() {
            eprintln!("x11 cursor helper error: {}", e);
            std::process::exit(1);
        }
        return;
    }
    let force_trace = args.iter().any(|a| a == "--trace");
    args.retain(|arg| arg != "--trace");
    init_logging(force_trace);

    if args.first().map(|s| s.as_str()) == Some("help")
        || args.iter().any(|a| a == "-h" || a == "--help")
    {
        print_help();
        return;
    }
    if args.first().map(|s| s.as_str()) == Some("version")
        || args.iter().any(|a| a == "-V" || a == "--version")
    {
        print_version();
        return;
    }

    if args.first().map(|s| s.as_str()) == Some("--daemon") {
        if let Err(e) = run_daemon().await {
            tracing::error!("daemon error: {}", e);
            std::process::exit(1);
        }
    } else if let Err(e) = run_cli_command(&args).await {
        tracing::error!("{}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod input_pipeline_tests {
    use super::{InputBatch, INPUT_QUEUE_BATCHES};
    use phantom::input::{InputEvent, Key, MouseMotionSource};
    use std::sync::Arc;
    use std::time::Duration;

    fn batch(sequence: usize) -> InputBatch {
        let event = match sequence % 3 {
            0 => InputEvent::KeyPress(Key::A),
            1 => InputEvent::MouseMove {
                dx: (sequence / 3) as i32,
                dy: -1,
                source: MouseMotionSource::Relative,
            },
            _ => InputEvent::KeyRelease(Key::A),
        };
        InputBatch {
            events: vec![event],
            event_timestamps: Vec::new(),
            syn_dropped_count: 0,
            queued_at: std::time::Instant::now(),
            routing_generation: 0,
        }
    }

    fn assert_event_matches_sequence(event: &InputEvent, sequence: usize) {
        match (sequence % 3, event) {
            (0, InputEvent::KeyPress(Key::A)) => {}
            (1, InputEvent::MouseMove { dx, dy: -1, .. }) => {
                assert_eq!(*dx, (sequence / 3) as i32);
            }
            (2, InputEvent::KeyRelease(Key::A)) => {}
            (_, other) => panic!("unexpected event at sequence {sequence}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bounded_pipeline_preserves_synthetic_125_to_8000_hz_streams() {
        for rate in [125, 500, 1_000, 8_000] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(INPUT_QUEUE_BATCHES);
            let producer = std::thread::spawn(move || {
                for sequence in 0..rate {
                    tx.blocking_send(batch(sequence)).unwrap();
                }
            });

            let mut received = 0;
            while received < rate {
                let input_batch = rx.recv().await.unwrap();
                assert_eq!(input_batch.events.len(), 1);
                assert_event_matches_sequence(&input_batch.events[0], received);
                received += 1;
            }
            producer.join().unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_backpressure_during_200_and_500_ms_backend_stalls_keeps_all_events() {
        for stall in [Duration::from_millis(200), Duration::from_millis(500)] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(INPUT_QUEUE_BATCHES);
            let producer = std::thread::spawn(move || {
                for sequence in 0..(INPUT_QUEUE_BATCHES * 2) {
                    tx.blocking_send(batch(sequence)).unwrap();
                }
            });

            tokio::time::sleep(stall).await;
            for sequence in 0..(INPUT_QUEUE_BATCHES * 2) {
                let input_batch = rx.recv().await.unwrap();
                assert_event_matches_sequence(&input_batch.events[0], sequence);
            }
            producer.join().unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn android_socket_stall_is_measured_at_ordered_endpoint_delivery() {
        for stall in [Duration::from_millis(200), Duration::from_millis(500)] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(INPUT_QUEUE_BATCHES);
            let count = 96;
            let producer = std::thread::spawn(move || {
                for sequence in 0..count {
                    tx.blocking_send(batch(sequence)).unwrap();
                }
            });

            let mut delivered = Vec::with_capacity(count);
            for sequence in 0..count {
                let input = rx.recv().await.expect("input event reaches output worker");
                // Model the blocking Android Socket write on the first event;
                // subsequent endpoint timestamps expose accumulated delay.
                if sequence == 0 {
                    tokio::time::sleep(stall).await;
                }
                assert_event_matches_sequence(&input.events[0], sequence);
                delivered.push((sequence, input.queued_at.elapsed()));
            }
            producer.join().unwrap();

            assert_eq!(delivered.len(), count);
            assert!(delivered[0].1 >= stall);
            assert!(delivered.last().unwrap().1 < stall + Duration::from_millis(150));
            assert!(delivered.windows(2).all(|pair| pair[0].0 < pair[1].0));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_receiver_unblocks_reader_for_graceful_shutdown() {
        let (tx, rx) = tokio::sync::mpsc::channel(INPUT_QUEUE_BATCHES);
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let reader = std::thread::spawn(move || {
            for sequence in 0..usize::MAX {
                if tx.blocking_send(batch(sequence)).is_err() {
                    break;
                }
            }
            worker_stopped.store(true, std::sync::atomic::Ordering::Release);
        });
        drop(rx);
        reader.join().unwrap();
        assert!(stopped.load(std::sync::atomic::Ordering::Acquire));
    }
}
