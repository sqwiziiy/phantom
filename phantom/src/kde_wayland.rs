use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use tokio::sync::{oneshot, watch};
use zbus::connection::Builder;

use crate::error::{PhantomError, Result};
use crate::mouse_touch::HostFrame;

const DBUS_NAME: &str = "org.phantom.Input";
const DBUS_PATH: &str = "/org/phantom/Input";

#[derive(Debug, Clone, PartialEq)]
pub struct KdeWindowSnapshot {
    pub resource_class: String,
    pub caption: String,
    pub frame: HostFrame,
    pub cursor_x: f64,
    pub cursor_y: f64,
    pub output_scale: f64,
}

impl KdeWindowSnapshot {
    pub fn is_waydroid(&self) -> bool {
        let class = self.resource_class.to_ascii_lowercase();
        let caption = self.caption.to_ascii_lowercase();
        class.contains("waydroid") || caption.contains("waydroid") || caption.contains("standoff 2")
    }
}

#[derive(Clone)]
struct SnapshotPublisher {
    tx: watch::Sender<Option<KdeWindowSnapshot>>,
}

#[zbus::interface(name = "org.phantom.Input")]
impl SnapshotPublisher {
    #[allow(clippy::too_many_arguments)]
    fn publish(
        &self,
        resource_class: String,
        caption: String,
        left: f64,
        top: f64,
        width: f64,
        height: f64,
        cursor_x: f64,
        cursor_y: f64,
        output_scale: f64,
    ) {
        let snapshot = (width.is_finite()
            && height.is_finite()
            && width > 0.0
            && height > 0.0
            && left.is_finite()
            && top.is_finite()
            && cursor_x.is_finite()
            && cursor_y.is_finite()
            && output_scale.is_finite()
            && output_scale > 0.0)
            .then_some(KdeWindowSnapshot {
                resource_class,
                caption,
                frame: HostFrame {
                    left,
                    top,
                    width,
                    height,
                },
                cursor_x,
                cursor_y,
                output_scale,
            });
        self.tx.send_replace(snapshot);
    }
}

pub struct KdeWaylandClient {
    receiver: watch::Receiver<Option<KdeWindowSnapshot>>,
    publisher_task: tokio::task::JoinHandle<()>,
    qdbus: PathBuf,
    plugin_id: String,
    script_path: PathBuf,
}

impl KdeWaylandClient {
    pub async fn start() -> Result<Option<Self>> {
        if env::var_os("WAYLAND_DISPLAY").is_none()
            || env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind != "wayland")
        {
            return Ok(None);
        }

        let Some(qdbus) = find_qdbus() else {
            tracing::info!("KDE Wayland coordinate helper unavailable (qdbus not found)");
            return Ok(None);
        };

        let (tx, receiver) = watch::channel(None);
        let (ready_tx, ready_rx) = oneshot::channel();
        let publisher_task = tokio::spawn(serve_snapshot_bus(tx, ready_tx));
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                publisher_task.abort();
                tracing::warn!(error = %err, "KWin coordinate D-Bus endpoint unavailable");
                return Ok(None);
            }
            Err(_) => {
                publisher_task.abort();
                return Ok(None);
            }
        }

        let plugin_id = format!("phantom-coordinates-{}", std::process::id());
        let script_path = env::temp_dir().join(format!("{plugin_id}.js"));
        fs::write(
            &script_path,
            include_str!("../resources/kwin_coordinates.js"),
        )?;

        if let Err(err) = call_qdbus(
            &qdbus,
            &[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.loadScript",
                script_path.to_str().unwrap_or_default(),
                &plugin_id,
            ],
        ) {
            let _ = fs::remove_file(&script_path);
            publisher_task.abort();
            tracing::warn!(error = %err, "failed to load the temporary KWin coordinate script");
            return Ok(None);
        }
        if let Err(err) = call_qdbus(
            &qdbus,
            &["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.start"],
        ) {
            let _ = call_qdbus(
                &qdbus,
                &[
                    "org.kde.KWin",
                    "/Scripting",
                    "org.kde.kwin.Scripting.unloadScript",
                    &plugin_id,
                ],
            );
            let _ = fs::remove_file(&script_path);
            publisher_task.abort();
            tracing::warn!(error = %err, "failed to start the temporary KWin coordinate script");
            return Ok(None);
        }

        tracing::info!("KWin Wayland window geometry and cursor tracking enabled");
        Ok(Some(Self {
            receiver,
            publisher_task,
            qdbus,
            plugin_id,
            script_path,
        }))
    }

    pub fn receiver(&self) -> watch::Receiver<Option<KdeWindowSnapshot>> {
        self.receiver.clone()
    }

    pub fn stop(self) {
        if let Err(err) = call_qdbus(
            &self.qdbus,
            &[
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.unloadScript",
                &self.plugin_id,
            ],
        ) {
            tracing::debug!(error = %err, "KWin coordinate script unload failed");
        }
        let _ = fs::remove_file(&self.script_path);
        self.publisher_task.abort();
    }
}

async fn serve_snapshot_bus(
    tx: watch::Sender<Option<KdeWindowSnapshot>>,
    ready: oneshot::Sender<std::result::Result<(), String>>,
) {
    let builder = match Builder::session() {
        Ok(builder) => builder,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    let builder = match builder
        .name(DBUS_NAME)
        .and_then(|builder| builder.serve_at(DBUS_PATH, SnapshotPublisher { tx }))
    {
        Ok(builder) => builder,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    match builder.build().await {
        Ok(connection) => {
            let _ = ready.send(Ok(()));
            let _connection = connection;
            std::future::pending::<()>().await;
        }
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
        }
    }
}

fn find_qdbus() -> Option<PathBuf> {
    ["qdbus6", "qdbus-qt6", "qdbus", "qdbus-qt5"]
        .iter()
        .find_map(|candidate| {
            Command::new(candidate)
                .arg("--version")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|_| PathBuf::from(candidate))
        })
}

fn call_qdbus(qdbus: &PathBuf, args: &[&str]) -> Result<String> {
    let output = Command::new(qdbus).args(args).output().map_err(|err| {
        PhantomError::Internal(format!("failed to invoke {}: {err}", qdbus.display()))
    })?;
    if !output.status.success() {
        return Err(PhantomError::Internal(format!(
            "{} failed: {}",
            qdbus.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn logical_to_android_pixel(
    frame: HostFrame,
    point_x: f64,
    point_y: f64,
    android_width: u32,
    android_height: u32,
) -> Option<(u32, u32)> {
    if !frame.left.is_finite()
        || !frame.top.is_finite()
        || !frame.width.is_finite()
        || !frame.height.is_finite()
        || frame.width <= 0.0
        || frame.height <= 0.0
        || !point_x.is_finite()
        || !point_y.is_finite()
        || android_width == 0
        || android_height == 0
    {
        return None;
    }
    let x = ((point_x - frame.left) / frame.width).clamp(0.0, 1.0);
    let y = ((point_y - frame.top) / frame.height).clamp(0.0, 1.0);
    Some((
        (x * (android_width - 1) as f64).round() as u32,
        (y * (android_height - 1) as f64).round() as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kde_logical_window_geometry_maps_to_android_pixels_at_common_scales() {
        let screen = (2560, 1440);
        for (scale, logical) in [
            (1.0, (2560.0, 1440.0)),
            (1.25, (2048.0, 1152.0)),
            (1.5, (2560.0 / 1.5, 1440.0 / 1.5)),
        ] {
            let frame = HostFrame {
                left: 320.0,
                top: 90.0,
                width: logical.0 - 320.0,
                height: logical.1 - 90.0,
            };
            let center = logical_to_android_pixel(
                frame,
                frame.left + frame.width / 2.0,
                frame.top + frame.height / 2.0,
                screen.0,
                screen.1,
            )
            .unwrap();
            assert_eq!(center, (1280, 720), "scale {scale}");
            assert_eq!(
                logical_to_android_pixel(
                    frame,
                    frame.left + frame.width,
                    frame.top + frame.height,
                    screen.0,
                    screen.1,
                ),
                Some((2559, 1439)),
                "scale {scale}"
            );
        }
    }

    #[test]
    fn coordinate_transform_clamps_to_client_bounds_and_rejects_bad_frames() {
        let frame = HostFrame {
            left: 100.0,
            top: 200.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(
            logical_to_android_pixel(frame, -500.0, 900.0, 2560, 1440),
            Some((0, 1439))
        );
        assert_eq!(
            logical_to_android_pixel(
                HostFrame {
                    width: 0.0,
                    ..frame
                },
                100.0,
                200.0,
                2560,
                1440
            ),
            None
        );
    }
}
