use std::time::Duration;

use tokio::process::Command;
use url::Url;

use crate::error::WmuError;

const CAPTIVE_PORTAL_PROBE: &str = "http://connectivitycheck.gstatic.com/generate_204";
const SSID: &str = "WMU Guest";

/// Fallback portal URL used when the probe response is clearly captive but we
/// can't extract a redirect from headers or body. Lets callers still attempt
/// the known WMU auth flow instead of silently exiting.
const FALLBACK_PORTAL_URL: &str = "https://legacy.wmich.edu/oit/guest/wmu-guest-policy.html\
    ?switch_url=https://virtual.wireless.wmich.edu/login.html\
    &ap_mac=00:00:00:00:00:00\
    &wlan=WMU%20Guest\
    &statusCode=1";

/// Browser-y UA string. Some captive portals return different / broken
/// responses to curl-style UAs (including non-redirecting 200 HTML).
const PROBE_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

#[derive(Debug)]
pub struct WifiState {
    pub interface: String,
    pub ssid: Option<String>,
    pub ip: Option<String>,
    pub connected: bool,
}

pub async fn get_wifi_state(interface: &str) -> Result<WifiState, WmuError> {
    let output = Command::new("nmcli")
        .args([
            "-t",
            "-f",
            "GENERAL.STATE,GENERAL.CONNECTION,IP4.ADDRESS",
            "device",
            "show",
            interface,
        ])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut ssid = None;
    let mut ip = None;
    let mut connected = false;

    for line in stdout.lines() {
        if let Some(val) = line.strip_prefix("GENERAL.CONNECTION:") {
            let val = val.trim();
            if !val.is_empty() && val != "--" {
                ssid = Some(val.to_string());
            }
        } else if let Some(val) = line.strip_prefix("GENERAL.STATE:") {
            connected = val.contains("connected") && !val.contains("disconnected");
        } else if let Some(val) = line.strip_prefix("IP4.ADDRESS[1]:") {
            ip = Some(val.trim().to_string());
        }
    }

    Ok(WifiState {
        interface: interface.to_string(),
        ssid,
        ip,
        connected,
    })
}

pub async fn disconnect(interface: &str) -> Result<(), WmuError> {
    let status = Command::new("nmcli")
        .args(["device", "disconnect", interface])
        .status()
        .await?;

    if !status.success() {
        return Err(WmuError::Wifi {
            msg: format!("nmcli disconnect failed (exit {})", status),
        });
    }
    Ok(())
}

pub async fn connect(interface: &str) -> Result<(), WmuError> {
    let status = Command::new("nmcli")
        .args(["device", "wifi", "connect", SSID, "ifname", interface])
        .status()
        .await?;

    if !status.success() {
        return Err(WmuError::Wifi {
            msg: format!("nmcli connect failed (exit {})", status),
        });
    }
    Ok(())
}

pub async fn wait_for_ip(interface: &str, timeout: Duration) -> Result<WifiState, WmuError> {
    let start = tokio::time::Instant::now();
    loop {
        let state = get_wifi_state(interface).await?;
        if state.connected && state.ip.is_some() {
            return Ok(state);
        }
        if start.elapsed() > timeout {
            return Err(WmuError::Wifi {
                msg: "timed out waiting for DHCP lease".to_string(),
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub async fn detect_wifi_interface() -> Option<String> {
    let output = Command::new("nmcli")
        .args(["-t", "-f", "DEVICE,TYPE,STATE", "device"])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[1] == "wifi" && parts[2].contains("connected") {
            return Some(parts[0].to_string());
        }
    }
    None
}

#[derive(Debug)]
pub enum ProbeResult {
    CaptivePortal { redirect_url: Url },
    Online,
    NoNetwork,
}

pub async fn detect_captive_portal() -> ProbeResult {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .timeout(Duration::from_secs(5))
        .user_agent(PROBE_USER_AGENT)
        .build()
        .unwrap();

    let resp = match client.get(CAPTIVE_PORTAL_PROBE).send().await {
        Ok(r) => r,
        Err(_) => return ProbeResult::NoNetwork,
    };

    let status = resp.status().as_u16();

    // Only HTTP 204 with empty body means truly online. This is the entire
    // contract of /generate_204 - any other status (including 200) indicates
    // a captive portal intercepted the request.
    if status == 204 {
        let len = resp.content_length().unwrap_or(0);
        if len == 0 {
            return ProbeResult::Online;
        }
    }

    // Check Location header on ANY non-204 status, not just 3xx. Some Cisco
    // WLC firmware returns HTTP 200 with a Location header pointing at the
    // policy page (observed in the wild: 200 OK + Location +
    // <meta http-equiv="refresh"> body). The strict 3xx gate missed this.
    if let Some(location) = resp.headers().get("location") {
        if let Ok(loc_str) = location.to_str() {
            if let Ok(url) = Url::parse(loc_str) {
                return ProbeResult::CaptivePortal { redirect_url: url };
            }
        }
    }

    let body = resp.text().await.unwrap_or_default();
    if let Some(url) = extract_meta_refresh(&body) {
        return ProbeResult::CaptivePortal { redirect_url: url };
    }

    if let Some(url) = extract_portal_url_from_body(&body) {
        return ProbeResult::CaptivePortal { redirect_url: url };
    }

    // Unknown non-204 response = captive, but we couldn't extract a redirect.
    // Fall back to the known WMU portal URL so auth can still be attempted,
    // rather than silently returning Online and leaving the user stranded.
    match Url::parse(FALLBACK_PORTAL_URL) {
        Ok(url) => ProbeResult::CaptivePortal { redirect_url: url },
        Err(_) => ProbeResult::NoNetwork,
    }
}

fn extract_meta_refresh(html: &str) -> Option<Url> {
    let lower = html.to_lowercase();
    let idx = lower.find("http-equiv=\"refresh\"")?;
    let rest = &html[idx..];
    let url_start = rest.to_lowercase().find("url=")?;
    let after_url = &rest[url_start + 4..];
    let end = after_url.find(['"', '\'', '>'])?;
    Url::parse(after_url[..end].trim()).ok()
}

fn extract_portal_url_from_body(html: &str) -> Option<Url> {
    let lower = html.to_lowercase();
    if let Some(idx) = lower.find("action=\"") {
        let rest = &html[idx + 8..];
        let end = rest.find('"')?;
        return Url::parse(&rest[..end]).ok();
    }
    for prefix in ["window.location", "location.href", "location.replace("] {
        if let Some(idx) = lower.find(prefix) {
            let rest = &html[idx..];
            let q_start = rest.find(['\'', '"'])?;
            let after_q = &rest[q_start + 1..];
            let q_end = after_q.find(['\'', '"'])?;
            if let Ok(url) = Url::parse(&after_q[..q_end]) {
                return Some(url);
            }
        }
    }
    None
}

pub async fn verify_connectivity() -> bool {
    matches!(detect_captive_portal().await, ProbeResult::Online)
}
