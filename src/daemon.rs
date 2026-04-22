use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::process::Command;
use zbus::Connection;
use zbus::fdo::PropertiesProxy;
use zbus::zvariant::OwnedObjectPath;

use crate::error::WmuError;
use crate::wifi;

const TARGET_SSID: &str = "WMU Guest";
const COOLDOWN: Duration = Duration::from_secs(15);
const SELF_POLL: Duration = Duration::from_secs(5);
const KEEPALIVE: Duration = Duration::from_secs(120);
const PROACTIVE_REAUTH: Duration = Duration::from_secs(15 * 60);
const CONN_FULL: u32 = 4;

const KEEPALIVE_URL: &str = "https://www.gstatic.com/generate_204";

/// Run the daemon event loop. Subscribes to two NM DBus property streams:
///
///   1. `Device.Wireless.ActiveAccessPoint` on every wireless device —
///      catches same-SSID AP roams. Dispatcher scripts do NOT fire on roam
///      (verified against /tmp/NetworkManager/src/core/nm-dispatcher.h);
///      NM emits the event only on DBus via set_current_ap() in
///      nm-device-wifi.c:2775.
///
///   2. `NetworkManager.Connectivity` — catches PORTAL transitions from
///      session timeouts (WLC revokes our MAC after inactivity), when NM's
///      own periodic connectivity check flips FULL → PORTAL.
///
/// On any trigger, invokes `wmu-guest-auth auto-auth` as a subprocess. Uses
/// a 30s cooldown to absorb roam storms at marginal AP boundaries.
pub async fn run() -> Result<(), WmuError> {
    log("starting daemon");
    let conn = Connection::system().await.map_err(dbus_err)?;
    log("connected to system DBus");

    let wifi_devices = discover_wifi_devices(&conn).await?;
    if wifi_devices.is_empty() {
        log("ERROR: no wifi devices found on DBus (is NetworkManager running?)");
        return Err(WmuError::Wifi {
            msg: "no wifi devices".to_string(),
        });
    }
    for dev in &wifi_devices {
        log(&format!("watching wifi device {}", dev.as_str()));
    }

    let mut triggers = futures_util::stream::SelectAll::new();

    for dev_path in &wifi_devices {
        let proxy = PropertiesProxy::builder(&conn)
            .destination("org.freedesktop.NetworkManager")
            .map_err(dbus_err)?
            .path(dev_path.clone())
            .map_err(dbus_err)?
            .build()
            .await
            .map_err(dbus_err)?;
        let stream = proxy.receive_properties_changed().await.map_err(dbus_err)?;
        triggers.push(stream.boxed());
        log(&format!(
            "subscribed to PropertiesChanged on {}",
            dev_path.as_str()
        ));
    }

    let nm_props = PropertiesProxy::builder(&conn)
        .destination("org.freedesktop.NetworkManager")
        .map_err(dbus_err)?
        .path("/org/freedesktop/NetworkManager")
        .map_err(dbus_err)?
        .build()
        .await
        .map_err(dbus_err)?;
    let nm_stream = nm_props
        .receive_properties_changed()
        .await
        .map_err(dbus_err)?;
    triggers.push(nm_stream.boxed());
    log("subscribed to PropertiesChanged on /org/freedesktop/NetworkManager");

    let mut last_trigger: Option<Instant> = None;

    // Startup self-check: run once immediately so we auth on daemon start
    // if we're already captive. Avoids waiting for the first event.
    log("startup self-check");
    if on_target_ssid(&conn).await
        && should_auth_now().await
        && try_trigger(&mut last_trigger, "startup self-check").await
    {
        log("startup self-check complete");
    }

    let mut poll_tick = tokio::time::interval(SELF_POLL);
    poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll_tick.tick().await;

    let mut keepalive_tick = tokio::time::interval(KEEPALIVE);
    keepalive_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive_tick.tick().await;

    let mut reauth_tick = tokio::time::interval(PROACTIVE_REAUTH);
    reauth_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    reauth_tick.tick().await;

    log(&format!(
        "event loop running (cooldown={}s, self-poll={}s, keepalive={}s, proactive-reauth={}s)",
        COOLDOWN.as_secs(),
        SELF_POLL.as_secs(),
        KEEPALIVE.as_secs(),
        PROACTIVE_REAUTH.as_secs()
    ));

    loop {
        tokio::select! {
            Some(signal) = triggers.next() => {
                let args = match signal.args() {
                    Ok(a) => a,
                    Err(e) => { log(&format!("signal parse error: {e}")); continue; }
                };
                let iface = args.interface_name();
                let changed: Vec<&str> = args.changed_properties().keys().copied().collect();
                log(&format!(
                    "event: iface={} props={:?}",
                    iface.as_str(),
                    changed
                ));

                let reason = if iface.as_str() == "org.freedesktop.NetworkManager.Device.Wireless" {
                    if !changed.contains(&"ActiveAccessPoint") { continue; }
                    Some("roam (ActiveAccessPoint changed)")
                } else if iface.as_str() == "org.freedesktop.NetworkManager" {
                    let Some(v) = args.changed_properties().get("Connectivity") else { continue };
                    let state: u32 = match v.downcast_ref::<u32>() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    log(&format!("Connectivity property → {state} ({})", conn_state_name(state)));
                    if state == CONN_FULL { continue; }
                    Some("connectivity dropped")
                } else {
                    None
                };

                let Some(reason) = reason else { continue };
                if !on_target_ssid(&conn).await {
                    log(&format!("{reason} but active SSID != {TARGET_SSID}, skipping"));
                    continue;
                }
                try_trigger(&mut last_trigger, reason).await;
            }
            _ = poll_tick.tick() => {
                if !on_target_ssid(&conn).await {
                    continue;
                }
                log("self-poll: checking is_truly_online");
                if should_auth_now().await {
                    try_trigger(&mut last_trigger, "self-poll detected captive").await;
                } else {
                    log("self-poll: confirmed online");
                }
            }
            _ = keepalive_tick.tick() => {
                if !on_target_ssid(&conn).await {
                    continue;
                }
                send_keepalive().await;
            }
            _ = reauth_tick.tick() => {
                if !on_target_ssid(&conn).await {
                    continue;
                }
                // Proactive reauth: fire regardless of current state to
                // refresh the WLC session before it can time out. If we're
                // still in RUN state this is a no-op on the WLC side
                // (buttonClicked=4 to an already-authed MAC is idempotent).
                // Bypasses the normal cooldown because this is scheduled,
                // not reactive.
                log("proactive reauth: refreshing WLC session");
                last_trigger = None;
                try_trigger(&mut last_trigger, "proactive reauth").await;
            }
        }
    }
}

async fn send_keepalive() {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let start = Instant::now();
    match client.head(KEEPALIVE_URL).send().await {
        Ok(r) => log(&format!(
            "keepalive HEAD {KEEPALIVE_URL} → {} in {}ms",
            r.status().as_u16(),
            start.elapsed().as_millis()
        )),
        Err(e) => log(&format!("keepalive failed ({e}) - WLC may have dropped us")),
    }
}

async fn should_auth_now() -> bool {
    !wifi::is_truly_online().await
}

async fn try_trigger(last: &mut Option<Instant>, reason: &str) -> bool {
    if let Some(t) = *last {
        if t.elapsed() < COOLDOWN {
            let remaining = COOLDOWN.saturating_sub(t.elapsed()).as_secs();
            log(&format!(
                "{reason} (cooldown {remaining}s remaining, skipping)"
            ));
            return false;
        }
    }
    *last = Some(Instant::now());
    log(&format!("{reason} → triggering auto-auth subprocess"));
    let start = Instant::now();
    let status = Command::new("/usr/local/bin/wmu-guest-auth")
        .args([
            "auto-auth",
            "--retries",
            "2",
            "--delay",
            "2",
            "--dhcp-timeout",
            "5",
        ])
        .status()
        .await;
    match status {
        Ok(s) => log(&format!(
            "auto-auth subprocess finished in {}ms exit={:?}",
            start.elapsed().as_millis(),
            s.code()
        )),
        Err(e) => log(&format!("auto-auth subprocess failed: {e}")),
    }
    true
}

fn log(msg: &str) {
    eprintln!("[wmu-guest-auth daemon {}] {msg}", chrono_now());
}

fn chrono_now() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("t={ts}")
}

fn conn_state_name(s: u32) -> &'static str {
    match s {
        0 => "UNKNOWN",
        1 => "NONE",
        2 => "PORTAL",
        3 => "LIMITED",
        4 => "FULL",
        _ => "?",
    }
}
async fn discover_wifi_devices(conn: &Connection) -> Result<Vec<OwnedObjectPath>, WmuError> {
    let nm = zbus::Proxy::new(
        conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await
    .map_err(dbus_err)?;

    let devices: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).await.map_err(dbus_err)?;

    let mut wifi = Vec::new();
    for path in devices {
        let dev = zbus::Proxy::new(
            conn,
            "org.freedesktop.NetworkManager",
            path.clone(),
            "org.freedesktop.NetworkManager.Device",
        )
        .await
        .map_err(dbus_err)?;
        let dev_type: u32 = match dev.get_property("DeviceType").await {
            Ok(v) => v,
            Err(_) => continue,
        };
        // NM_DEVICE_TYPE_WIFI = 2
        if dev_type == 2 {
            wifi.push(path);
        }
    }
    Ok(wifi)
}

async fn on_target_ssid(conn: &Connection) -> bool {
    let Ok(nm) = zbus::Proxy::new(
        conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await
    else {
        return false;
    };
    let active: Vec<OwnedObjectPath> = match nm.get_property("ActiveConnections").await {
        Ok(v) => v,
        Err(_) => return false,
    };
    for path in active {
        let Ok(ac) = zbus::Proxy::new(
            conn,
            "org.freedesktop.NetworkManager",
            path,
            "org.freedesktop.NetworkManager.Connection.Active",
        )
        .await
        else {
            continue;
        };
        let id: String = match ac.get_property("Id").await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if id == TARGET_SSID {
            return true;
        }
    }
    false
}

fn dbus_err(e: impl std::fmt::Display) -> WmuError {
    WmuError::Wifi {
        msg: format!("dbus: {e}"),
    }
}
