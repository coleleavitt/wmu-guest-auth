use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use zbus::Connection;
use zbus::fdo::PropertiesProxy;
use zbus::zvariant::OwnedObjectPath;

use crate::error::WmuError;
use crate::wifi;

const TARGET_SSID: &str = "WMU Guest";
const COOLDOWN: Duration = Duration::from_secs(15);
const KEEPALIVE: Duration = Duration::from_secs(120);
const PROACTIVE_REAUTH: Duration = Duration::from_secs(15 * 60);
const CONN_FULL: u32 = 4;

/// If the main event loop hasn't made progress in this long, exit so
/// supervise-daemon respawns us. Defends against two observed failure modes:
///  - Tokio timers wedged after laptop suspend/resume (monotonic-clock edge
///    case; reproduced at t=2301 → 59min gap in /var/log/wmu-guest-auth.log).
///  - zbus SignalStream terminates silently on DBus disconnect. Per zbus
///    5.15 source (proxy/mod.rs:1325), SignalStream is a FusedStream that
///    returns None forever after terminate — no auto-reconnect.
///
/// Must be > 2 × max(KEEPALIVE, POLL_IDLE) to avoid false restarts.
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(300);
const WATCHDOG_CHECK_INTERVAL: Duration = Duration::from_secs(60);

const KEEPALIVE_URL: &str = "https://www.gstatic.com/generate_204";

/// Adaptive self-poll intervals. When the last auth was recent or the
/// connection is unstable, poll aggressively (5s). After N consecutive
/// "confirmed online" polls, back off to reduce wakeups / battery drain.
/// Resets to FAST on any observed captive state or roam event.
const POLL_FAST: Duration = Duration::from_secs(5);
const POLL_STABLE: Duration = Duration::from_secs(15);
const POLL_IDLE: Duration = Duration::from_secs(45);
const STABLE_THRESHOLD: u32 = 6;
const IDLE_THRESHOLD: u32 = 24;

/// Run the daemon event loop. Subscribes to two NM DBus property streams:
///
///   1. `Device.Wireless.ActiveAccessPoint` — catches same-SSID roams.
///      Dispatcher scripts do NOT fire on roam (verified in
///      /tmp/NetworkManager/src/core/nm-dispatcher.h); NM emits only via
///      DBus in nm-device-wifi.c:2775.
///   2. `NetworkManager.Connectivity` — catches PORTAL transitions.
///
/// Plus three timers: adaptive self-poll (5s→15s→45s), keepalive (120s,
/// keeps WLC idle timer from firing), proactive reauth (15min, refreshes
/// WLC session before it times out).
///
/// All auth work happens in-process via crate::cmd_auto_auth — no subprocess
/// spawning, no double dispatch, ~50ms faster per trigger.
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

    let mut state = DaemonState::default();

    log("startup self-check");
    if on_target_ssid(&conn).await
        && should_auth_now().await
        && try_trigger(&mut state, "startup self-check").await
    {
        log("startup self-check complete");
    }

    let mut poll_tick = tokio::time::interval(POLL_FAST);
    poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll_tick.tick().await;

    let mut keepalive_tick = tokio::time::interval(KEEPALIVE);
    keepalive_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive_tick.tick().await;

    let mut reauth_tick = tokio::time::interval(PROACTIVE_REAUTH);
    reauth_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    reauth_tick.tick().await;

    log(&format!(
        "event loop running (cooldown={}s, poll=adaptive[{}/{}/{}]s, keepalive={}s, proactive-reauth={}s, watchdog={}s)",
        COOLDOWN.as_secs(),
        POLL_FAST.as_secs(),
        POLL_STABLE.as_secs(),
        POLL_IDLE.as_secs(),
        KEEPALIVE.as_secs(),
        PROACTIVE_REAUTH.as_secs(),
        WATCHDOG_TIMEOUT.as_secs(),
    ));

    let heartbeat = Arc::new(AtomicU64::new(unix_now()));
    spawn_watchdog(Arc::clone(&heartbeat));

    loop {
        heartbeat.store(unix_now(), Ordering::Relaxed);
        tokio::select! {
            Some(signal) = triggers.next() => {
                let args = match signal.args() {
                    Ok(a) => a,
                    Err(e) => { log(&format!("signal parse error: {e}")); continue; }
                };
                let iface = args.interface_name();
                let changed: Vec<&str> = args.changed_properties().keys().copied().collect();

                let reason = if iface.as_str() == "org.freedesktop.NetworkManager.Device.Wireless" {
                    if !changed.contains(&"ActiveAccessPoint") { continue; }
                    Some("roam (ActiveAccessPoint changed)")
                } else if iface.as_str() == "org.freedesktop.NetworkManager" {
                    let Some(v) = args.changed_properties().get("Connectivity") else { continue };
                    let state_val: u32 = match v.downcast_ref::<u32>() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    log(&format!(
                        "Connectivity → {state_val} ({})",
                        conn_state_name(state_val)
                    ));
                    if state_val == CONN_FULL { continue; }
                    Some("connectivity dropped")
                } else {
                    None
                };

                let Some(reason) = reason else { continue };
                if !on_target_ssid(&conn).await {
                    log(&format!("{reason} but active SSID != {TARGET_SSID}, skipping"));
                    continue;
                }
                // DBus event implies volatility — reset poll to fast.
                state.clean_polls = 0;
                set_poll_interval(&mut poll_tick, state.current_poll_interval());
                try_trigger(&mut state, reason).await;
            }
            _ = poll_tick.tick() => {
                if !on_target_ssid(&conn).await { continue; }
                log(&format!(
                    "self-poll (clean_polls={}, interval={}s)",
                    state.clean_polls,
                    state.current_poll_interval().as_secs()
                ));
                if should_auth_now().await {
                    let drop_gap = state.record_captive();
                    if let Some(gap) = drop_gap {
                        log(&format!(
                            "WLC session observation: captive after {}s of continuous online (possible WLC session/idle timeout)",
                            gap.as_secs()
                        ));
                    }
                    try_trigger(&mut state, "self-poll detected captive").await;
                    set_poll_interval(&mut poll_tick, POLL_FAST);
                } else {
                    state.record_clean_poll();
                    let new_interval = state.current_poll_interval();
                    log(&format!(
                        "self-poll: online (clean_polls={}, next poll in {}s)",
                        state.clean_polls,
                        new_interval.as_secs()
                    ));
                    set_poll_interval(&mut poll_tick, new_interval);
                }
            }
            _ = keepalive_tick.tick() => {
                if !on_target_ssid(&conn).await { continue; }
                send_keepalive().await;
            }
            _ = reauth_tick.tick() => {
                if !on_target_ssid(&conn).await { continue; }
                // Proactive reauth bypasses cooldown — it's scheduled,
                // not reactive. buttonClicked=4 on an already-authed MAC
                // is a no-op on the WLC side (idempotent). Refreshes the
                // session clock before WLC timeout can fire.
                log("proactive reauth: refreshing WLC session");
                state.last_trigger = None;
                try_trigger(&mut state, "proactive reauth").await;
            }
        }
    }
}

/// Mutable state the daemon carries across events. Tracks adaptive-poll
/// counters, cooldown timer, and the moment of last confirmed-online for
/// session-lifetime observation.
struct DaemonState {
    last_trigger: Option<Instant>,
    last_confirmed_online: Option<Instant>,
    clean_polls: u32,
    was_captive_last_poll: bool,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            last_trigger: None,
            last_confirmed_online: None,
            clean_polls: 0,
            was_captive_last_poll: false,
        }
    }
}

impl DaemonState {
    fn current_poll_interval(&self) -> Duration {
        if self.clean_polls >= IDLE_THRESHOLD {
            POLL_IDLE
        } else if self.clean_polls >= STABLE_THRESHOLD {
            POLL_STABLE
        } else {
            POLL_FAST
        }
    }

    fn record_clean_poll(&mut self) {
        self.clean_polls = self.clean_polls.saturating_add(1);
        self.last_confirmed_online = Some(Instant::now());
        self.was_captive_last_poll = false;
    }

    /// Called when self-poll detects captive. Returns the gap since the
    /// last confirmed-online, if known — this is a good approximation of
    /// the WLC's session/idle timeout, useful for tuning PROACTIVE_REAUTH.
    fn record_captive(&mut self) -> Option<Duration> {
        let gap = if self.was_captive_last_poll {
            None
        } else {
            self.last_confirmed_online.map(|t| t.elapsed())
        };
        self.clean_polls = 0;
        self.was_captive_last_poll = true;
        gap
    }
}

fn set_poll_interval(tick: &mut tokio::time::Interval, new: Duration) {
    if tick.period() != new {
        *tick = tokio::time::interval_at(tokio::time::Instant::now() + new, new);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
            "keepalive → {} in {}ms",
            r.status().as_u16(),
            start.elapsed().as_millis()
        )),
        Err(e) => log(&format!("keepalive failed ({e}) — WLC may have dropped us")),
    }
}

async fn should_auth_now() -> bool {
    !wifi::is_truly_online().await
}

async fn try_trigger(state: &mut DaemonState, reason: &str) -> bool {
    if let Some(t) = state.last_trigger {
        if t.elapsed() < COOLDOWN {
            let remaining = COOLDOWN.saturating_sub(t.elapsed()).as_secs();
            log(&format!("{reason} (cooldown {remaining}s, skipping)"));
            return false;
        }
    }
    state.last_trigger = Some(Instant::now());
    log(&format!("{reason} → in-process auto-auth starting"));
    let start = Instant::now();
    // retries=4, delay=3: ~45s total budget. Tuned from real campus logs
    // at t=2003-2301 where recovery took 19-30s per attempt; the previous
    // 2-retry × 2s delay budget ran out in ~15s and gave up while the
    // network was still stabilizing, then kept retrying with a 15s
    // cooldown gap in between. Net user-visible captive time was 60+s
    // instead of the ~20s needed to actually recover.
    match crate::cmd_auto_auth(4, 3, None, 5).await {
        Ok(()) => log(&format!(
            "auto-auth succeeded in {}ms",
            start.elapsed().as_millis()
        )),
        Err(e) => log(&format!(
            "auto-auth failed in {}ms: {e}",
            start.elapsed().as_millis()
        )),
    }
    true
}

fn log(msg: &str) {
    eprintln!("[wmu-guest-auth daemon {}] {msg}", chrono_now());
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Spawn a separate tokio task that checks the heartbeat every minute.
/// If the main loop has not updated it within WATCHDOG_TIMEOUT, exit the
/// process. supervise-daemon will respawn us with a fresh zbus Connection
/// and a fresh tokio runtime - recovering from the hangs we saw at
/// t=2301→3556s and any future DBus/timer wedge.
///
/// Uses SystemTime (wall clock) not tokio::Instant, because the whole
/// point is that tokio timers may themselves be wedged.
fn spawn_watchdog(heartbeat: Arc<AtomicU64>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(WATCHDOG_CHECK_INTERVAL).await;
            let last = heartbeat.load(Ordering::Relaxed);
            let now = unix_now();
            let age = now.saturating_sub(last);
            if age > WATCHDOG_TIMEOUT.as_secs() {
                log(&format!(
                    "WATCHDOG: event loop silent for {age}s (limit {}s) — exiting for respawn",
                    WATCHDOG_TIMEOUT.as_secs()
                ));
                std::process::exit(42);
            }
        }
    });
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
