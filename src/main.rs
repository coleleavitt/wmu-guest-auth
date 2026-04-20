#![allow(dead_code)]

mod assets;
mod auth;
mod dns;
mod error;
mod portal;
mod wifi;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use colored::Colorize;
use url::Url;

use crate::error::WmuError;
use crate::wifi::ProbeResult;

const DEFAULT_PORTAL_URL: &str = "https://legacy.wmich.edu/oit/guest/wmu-guest-policy.html\
    ?switch_url=https://virtual.wireless.wmich.edu/login.html\
    &ap_mac=00:81:c4:75:63:e0\
    &wlan=WMU%20Guest\
    &statusCode=1";

const DEFAULT_INTERFACE: &str = "wlp132s0f0";

#[derive(Parser)]
#[command(
    name = "wmu-guest-auth",
    about = "WMU Guest WiFi captive portal RE and auth replay tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Auto-connect: disconnect → reconnect → detect portal → authenticate
    Connect {
        #[arg(short, long, default_value = DEFAULT_INTERFACE)]
        interface: String,
        #[arg(short, long, default_value = "./wmu-dump")]
        output: PathBuf,
        /// Skip disconnect (just detect portal + auth from current state)
        #[arg(long)]
        no_reconnect: bool,
    },
    Auth {
        #[arg(short, long)]
        url: Option<String>,
    },
    Deauth {
        #[arg(short, long, default_value = "https://virtual.wireless.wmich.edu")]
        wlc_url: String,
    },
    Recon {
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
    },
    Dump {
        #[arg(short, long)]
        url: Option<String>,
        #[arg(short, long, default_value = "./wmu-dump")]
        output: PathBuf,
    },
    Full {
        #[arg(short, long)]
        url: Option<String>,
        #[arg(short, long, default_value = "./wmu-dump")]
        output: PathBuf,
    },
    /// Headless auto-auth: detect captive portal → POST accept → exit. Designed for NM dispatcher.
    AutoAuth {
        #[arg(short, long, default_value = "5")]
        retries: u8,
        #[arg(short, long, default_value = "3")]
        delay: u64,
        /// Interface to wait for DHCP on before probing. If unset, the tool
        /// tries to auto-detect the first active wifi device via nmcli.
        #[arg(short, long)]
        interface: Option<String>,
        /// Max seconds to wait for a DHCP lease before starting the probe loop.
        #[arg(long, default_value = "30")]
        dhcp_timeout: u64,
    },
}

#[tokio::main]
async fn main() -> Result<(), WmuError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Connect {
            interface,
            output,
            no_reconnect,
        } => cmd_connect(&interface, &output, no_reconnect).await,
        Commands::Auth { url } => cmd_auth(url).await,
        Commands::Deauth { wlc_url } => cmd_deauth(&wlc_url).await,
        Commands::Recon { output } => cmd_recon(&output).await,
        Commands::Dump { url, output } => cmd_dump(url, &output).await,
        Commands::Full { url, output } => cmd_full(url, &output).await,
        Commands::AutoAuth {
            retries,
            delay,
            interface,
            dhcp_timeout,
        } => cmd_auto_auth(retries, delay, interface, dhcp_timeout).await,
    }
}

async fn cmd_connect(
    interface: &str,
    output: &PathBuf,
    no_reconnect: bool,
) -> Result<(), WmuError> {
    println!("{}", "[ WMU Guest Auto-Connect ]".bold().cyan());
    println!("  interface: {}", interface);

    let mut log = RequestLog::new();

    print_phase(1, "Current State");
    let state = wifi::get_wifi_state(interface).await?;
    println!(
        "  connected: {} | ssid: {} | ip: {}",
        if state.connected {
            "yes".green()
        } else {
            "no".red()
        },
        state.ssid.as_deref().unwrap_or("-").yellow(),
        state.ip.as_deref().unwrap_or("-")
    );

    if !no_reconnect {
        print_phase(2, "Disconnect");
        if state.connected {
            print_step("Disconnecting...");
            wifi::disconnect(interface).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            println!("  {}", "disconnected".yellow());
        } else {
            println!("  already disconnected");
        }

        print_phase(3, "Reconnect");
        print_step("Connecting to WMU Guest...");
        wifi::connect(interface).await?;

        print_step("Waiting for DHCP lease...");
        let new_state = wifi::wait_for_ip(interface, Duration::from_secs(15)).await?;
        println!(
            "  {} | ip: {}",
            "associated".green(),
            new_state.ip.as_deref().unwrap_or("-")
        );
    } else {
        println!("  (skipping reconnect)");
    }

    print_phase(4, "Portal Detection");
    print_step("Probing connectivity...");

    let mut portal_url = None;
    for attempt in 1..=5 {
        let probe = wifi::detect_captive_portal().await;
        match &probe {
            ProbeResult::CaptivePortal { redirect_url } => {
                log.push(
                    "PROBE",
                    "GET",
                    CAPTIVE_PORTAL_PROBE_URL,
                    302,
                    "→ captive portal redirect",
                );
                println!(
                    "  {} captive portal detected (attempt {})",
                    "→".green(),
                    attempt
                );
                println!("  redirect: {}", redirect_url.to_string().yellow());
                portal_url = Some(redirect_url.clone());
                break;
            }
            ProbeResult::Online => {
                log.push(
                    "PROBE",
                    "GET",
                    CAPTIVE_PORTAL_PROBE_URL,
                    204,
                    "already online",
                );
                println!(
                    "  {} already online - no captive portal (attempt {})",
                    "✓".green(),
                    attempt
                );
                println!(
                    "\n  You're already authenticated. Run {} to start fresh.",
                    "wmu-guest-auth connect".bold()
                );
                save_log(&log, output).await?;
                return Ok(());
            }
            ProbeResult::NoNetwork => {
                log.push("PROBE", "GET", CAPTIVE_PORTAL_PROBE_URL, 0, "no network");
                if attempt < 5 {
                    println!(
                        "  {} no network yet, retrying ({}/5)...",
                        "⏳".yellow(),
                        attempt
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                } else {
                    println!("  {} no network after 5 attempts", "✗".red());
                    save_log(&log, output).await?;
                    return Err(WmuError::Wifi {
                        msg: "no network connectivity after reconnect".to_string(),
                    });
                }
            }
        }
    }

    let portal_url = match portal_url {
        Some(u) => u,
        None => {
            println!("  no captive portal detected, using default URL");
            Url::parse(DEFAULT_PORTAL_URL)?
        }
    };

    print_phase(5, "Portal Fetch");
    print_step(&format!("GET {}", portal_url));
    let page = portal::fetch_portal(&portal_url).await?;
    log.push(
        "PORTAL",
        "GET",
        portal_url.as_str(),
        200,
        &format!("{} bytes", page.html.len()),
    );

    println!(
        "  switch_url: {}",
        page.params.switch_url.to_string().yellow()
    );
    println!("  ap_mac:     {}", page.params.ap_mac);
    println!("  wlan:       {}", page.params.wlan);
    println!("  assets:     {}", page.asset_urls.len());

    assets::save_html(&page.html, &output.join("html/portal.html")).await?;

    print_step(&format!("GET {}", page.params.switch_url));
    let (wlc_html, wlc_assets) = portal::fetch_wlc_page(&page.params.switch_url).await?;
    log.push(
        "WLC",
        "GET",
        page.params.switch_url.as_str(),
        200,
        &format!("{} bytes", wlc_html.len()),
    );
    assets::save_html(&wlc_html, &output.join("html/wlc-login.html")).await?;

    let mut all_assets = page.asset_urls.clone();
    all_assets.extend(wlc_assets);
    if !all_assets.is_empty() {
        print_step(&format!("Downloading {} assets...", all_assets.len()));
        let downloaded = assets::download_all(&all_assets, output).await?;
        for d in &downloaded {
            log.push(
                "ASSET",
                "GET",
                d.asset.url.as_str(),
                200,
                &format!("{}", format_size(d.size)),
            );
            println!(
                "    {} {} ({})",
                d.asset.kind.to_string().dimmed(),
                d.path.display(),
                format_size(d.size)
            );
        }
    }

    print_phase(6, "Auth Replay");
    print_step(&format!(
        "POST {} [buttonClicked=4]",
        page.params.switch_url
    ));
    let result = auth::authenticate(&page.params).await?;
    log.push(
        "AUTH",
        "POST",
        page.params.switch_url.as_str(),
        result.status,
        if result.success {
            "authenticated"
        } else {
            "failed"
        },
    );

    if result.success {
        println!("  {}", "authenticated!".bold().green());
        if let Some(ref logout) = result.logout_url {
            println!("  logout: {}", logout);
        }
    } else {
        println!("  {} (HTTP {})", "auth POST sent".yellow(), result.status);
    }

    assets::save_html(
        &result.response_body,
        &output.join("html/auth-response.html"),
    )
    .await?;

    print_phase(7, "Verify Connectivity");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let online = wifi::verify_connectivity().await;
    if online {
        log.push("VERIFY", "GET", CAPTIVE_PORTAL_PROBE_URL, 204, "online");
        println!("  {}", "internet access confirmed!".bold().green());
    } else {
        log.push(
            "VERIFY",
            "GET",
            CAPTIVE_PORTAL_PROBE_URL,
            0,
            "still captive",
        );
        println!(
            "  {} still no internet - auth may not have taken effect",
            "⚠".yellow()
        );
    }

    save_log(&log, output).await?;

    println!("\n{}", "Done.".bold().green());
    println!("  output:   {}", output.display());
    println!("  requests: {}", log.entries.len());

    Ok(())
}

const CAPTIVE_PORTAL_PROBE_URL: &str = "http://connectivitycheck.gstatic.com/generate_204";

async fn cmd_auto_auth(
    retries: u8,
    delay: u64,
    interface: Option<String>,
    dhcp_timeout: u64,
) -> Result<(), WmuError> {
    let delay = Duration::from_secs(delay);

    // Wait for a DHCP lease before probing. Without this, the NM dispatcher
    // runs the tool on "action=up" which fires BEFORE DHCP completes; probes
    // fail with NoNetwork for 40s then the tool gives up and NM never
    // re-triggers. The user is left stranded behind the captive portal.
    let iface = match interface {
        Some(i) => Some(i),
        None => wifi::detect_wifi_interface().await,
    };
    if let Some(iface) = iface {
        eprintln!("wmu-guest-auth: waiting up to {dhcp_timeout}s for DHCP on {iface}");
        match wifi::wait_for_ip(&iface, Duration::from_secs(dhcp_timeout)).await {
            Ok(state) => eprintln!(
                "wmu-guest-auth: got lease ip={} on {iface}",
                state.ip.as_deref().unwrap_or("-")
            ),
            Err(e) => eprintln!("wmu-guest-auth: dhcp wait warning: {e} (continuing anyway)"),
        }
    } else {
        eprintln!("wmu-guest-auth: no wifi interface detected, skipping DHCP wait");
    }

    for attempt in 1..=retries {
        tokio::time::sleep(delay).await;

        match wifi::detect_captive_portal().await {
            ProbeResult::Online => {
                eprintln!("wmu-guest-auth: already online (attempt {attempt})");
                return Ok(());
            }
            ProbeResult::NoNetwork => {
                eprintln!("wmu-guest-auth: no network (attempt {attempt}/{retries})");
                continue;
            }
            ProbeResult::CaptivePortal { redirect_url } => {
                eprintln!("wmu-guest-auth: captive portal detected → {redirect_url}");

                let portal_url = redirect_url;
                match portal::fetch_portal(&portal_url).await {
                    Ok(page) => {
                        eprintln!(
                            "wmu-guest-auth: switch_url={} ap_mac={}",
                            page.params.switch_url, page.params.ap_mac
                        );
                        match auth::authenticate(&page.params).await {
                            Ok(result) => {
                                eprintln!(
                                    "wmu-guest-auth: POST buttonClicked=4 → HTTP {} (body-success={})",
                                    result.status, result.success
                                );
                            }
                            Err(e) => {
                                eprintln!("wmu-guest-auth: auth POST failed: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("wmu-guest-auth: portal fetch failed ({e}), trying direct auth");
                        let params = portal::WlcParams::direct_default();
                        if let Err(e) = auth::authenticate(&params).await {
                            eprintln!("wmu-guest-auth: direct auth POST failed: {e}");
                        }
                    }
                }

                // Connectivity re-probe is the only source of truth. The
                // auth POST body may say success but the client MAC may
                // not be in the WLC's authed list yet (propagation lag).
                // Poll for up to 5s.
                let mut confirmed = false;
                for verify_attempt in 1..=5 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if matches!(wifi::detect_captive_portal().await, ProbeResult::Online) {
                        eprintln!(
                            "wmu-guest-auth: online! (verified {verify_attempt}s after auth)"
                        );
                        confirmed = true;
                        break;
                    }
                }
                if confirmed {
                    return Ok(());
                }
                eprintln!("wmu-guest-auth: still captive after auth (attempt {attempt}/{retries})");
            }
        }
    }

    eprintln!("wmu-guest-auth: gave up after {retries} attempts");
    Err(WmuError::Wifi {
        msg: "auto-auth failed".to_string(),
    })
}

async fn cmd_auth(url: Option<String>) -> Result<(), WmuError> {
    let portal_url = Url::parse(url.as_deref().unwrap_or(DEFAULT_PORTAL_URL))?;

    println!("{}", "[ WMU Guest Auth ]".bold().cyan());
    println!("  portal: {}", portal_url);

    print_step("Fetching portal page...");
    let page = portal::fetch_portal(&portal_url).await?;

    println!("  switch_url: {}", page.params.switch_url);
    println!("  ap_mac:     {}", page.params.ap_mac);
    println!("  wlan:       {}", page.params.wlan);
    println!("  status:     {}", page.params.status_code);

    print_step("Sending buttonClicked=4 to WLC...");
    let result = auth::authenticate(&page.params).await?;

    if result.success {
        println!("  {}", "authenticated!".bold().green());
        if let Some(ref logout) = result.logout_url {
            println!("  logout_url: {}", logout);
        }
    } else {
        println!("  {} (HTTP {})", "auth failed".bold().red(), result.status);
    }

    Ok(())
}

async fn cmd_deauth(wlc_url: &str) -> Result<(), WmuError> {
    let url = Url::parse(wlc_url)?;
    println!("{}", "[ WMU Guest Deauth ]".bold().cyan());

    print_step("Sending logout POST...");
    let result = auth::deauthenticate(&url).await?;
    println!("  status: {} (HTTP {})", "sent".green(), result.status);

    Ok(())
}

async fn cmd_recon(output: &PathBuf) -> Result<(), WmuError> {
    println!("{}", "[ WMU DNS Recon ]".bold().cyan());

    print_step("Running DNS queries...");
    let report = dns::run_recon().await?;

    let mut current_domain = String::new();
    for record in &report.records {
        if record.name != current_domain {
            current_domain.clone_from(&record.name);
            println!("\n  {}", current_domain.bold().yellow());
        }
        println!("    {:6} {}", record.record_type.dimmed(), record.value);
    }

    let dns_file = output.join("dns-records.txt");
    let mut content = String::from("WMU DNS Reconnaissance\n\n");
    for record in &report.records {
        content.push_str(&format!(
            "{}\t{}\t{}\n",
            record.name, record.record_type, record.value
        ));
    }
    tokio::fs::create_dir_all(output).await?;
    tokio::fs::write(&dns_file, &content).await?;
    println!("\n  saved to {}", dns_file.display().to_string().dimmed());

    println!(
        "\n  {} records across {} domains",
        report.records.len().to_string().bold(),
        DOMAIN_COUNT.to_string().bold()
    );

    Ok(())
}

const DOMAIN_COUNT: usize = 8;

async fn cmd_dump(url: Option<String>, output: &PathBuf) -> Result<(), WmuError> {
    let portal_url = Url::parse(url.as_deref().unwrap_or(DEFAULT_PORTAL_URL))?;

    println!("{}", "[ WMU Portal Dump ]".bold().cyan());

    print_step("Fetching portal page...");
    let page = portal::fetch_portal(&portal_url).await?;
    let portal_dest = output.join("html/portal.html");
    assets::save_html(&page.html, &portal_dest).await?;
    println!("  saved portal HTML ({} bytes)", page.html.len());

    print_step("Fetching WLC login page...");
    let (wlc_html, wlc_assets) = portal::fetch_wlc_page(&page.params.switch_url).await?;
    let wlc_dest = output.join("html/wlc-login.html");
    assets::save_html(&wlc_html, &wlc_dest).await?;
    println!("  saved WLC login HTML ({} bytes)", wlc_html.len());

    let mut all_assets = page.asset_urls.clone();
    all_assets.extend(wlc_assets);

    print_step(&format!("Downloading {} assets...", all_assets.len()));
    let downloaded = assets::download_all(&all_assets, output).await?;
    for d in &downloaded {
        println!(
            "    {} {} ({})",
            d.asset.kind.to_string().dimmed(),
            d.path.display(),
            format_size(d.size)
        );
    }

    println!(
        "\n  {} assets saved to {}",
        downloaded.len().to_string().bold(),
        output.display().to_string().dimmed()
    );

    Ok(())
}

async fn cmd_full(url: Option<String>, output: &PathBuf) -> Result<(), WmuError> {
    let portal_url = Url::parse(url.as_deref().unwrap_or(DEFAULT_PORTAL_URL))?;

    println!("{}", "[ WMU Full RE + Auth ]".bold().cyan());
    println!();

    print_phase(1, "DNS Recon");
    let report = dns::run_recon().await?;
    let mut current_domain = String::new();
    for record in &report.records {
        if record.name != current_domain {
            current_domain.clone_from(&record.name);
            println!("  {}", current_domain.bold().yellow());
        }
        println!("    {:6} {}", record.record_type.dimmed(), record.value);
    }
    println!("  {} total records\n", report.records.len());

    print_phase(2, "Portal Analysis");
    let page = portal::fetch_portal(&portal_url).await?;
    println!("  switch_url: {}", page.params.switch_url);
    println!("  ap_mac:     {}", page.params.ap_mac);
    println!("  wlan:       {}", page.params.wlan);
    println!("  assets:     {}", page.asset_urls.len());

    let (wlc_html, wlc_assets) = portal::fetch_wlc_page(&page.params.switch_url).await?;
    println!("  wlc assets: {}\n", wlc_assets.len());

    print_phase(3, "Asset Download");
    assets::save_html(&page.html, &output.join("html/portal.html")).await?;
    assets::save_html(&wlc_html, &output.join("html/wlc-login.html")).await?;

    let mut all_assets = page.asset_urls.clone();
    all_assets.extend(wlc_assets);
    let downloaded = assets::download_all(&all_assets, output).await?;
    for d in &downloaded {
        println!(
            "    {} {} ({})",
            d.asset.kind.to_string().dimmed(),
            d.path.display(),
            format_size(d.size)
        );
    }
    println!();

    print_phase(4, "Auth Flow");
    println!("  protocol:  Cisco WLC Web Authentication (consent/webpassthrough)");
    println!("  method:    POST to switch_url with buttonClicked=4");
    println!("  creds:     none (accept-only, no username/password)");
    println!("  binding:   MAC-based (client MAC registered on WLC)");
    println!("  logout:    POST to /logout.html with userStatus=1");
    println!(
        "  wlc vip:   {} (Cisco virtual interface, non-routable)",
        page.params.switch_url.host_str().unwrap_or("unknown")
    );
    println!();

    print_phase(5, "Auth Replay");
    let result = auth::authenticate(&page.params).await?;
    if result.success {
        println!("  {}", "authenticated!".bold().green());
        if let Some(ref logout) = result.logout_url {
            println!("  logout: {}", logout);
        }
    } else {
        println!("  {} (HTTP {})", "auth failed".bold().red(), result.status);
        println!("  (this is expected if you're not on WMU Guest WiFi)");
    }

    let dns_file = output.join("dns-records.txt");
    let mut content = String::from("WMU DNS Reconnaissance\n\n");
    for record in &report.records {
        content.push_str(&format!(
            "{}\t{}\t{}\n",
            record.name, record.record_type, record.value
        ));
    }
    tokio::fs::create_dir_all(output).await?;
    tokio::fs::write(&dns_file, &content).await?;

    println!("\n{}", "Done.".bold().green());
    println!("  output: {}", output.display());

    Ok(())
}

struct RequestLog {
    entries: Vec<LogEntry>,
}

struct LogEntry {
    phase: String,
    method: String,
    url: String,
    status: u16,
    note: String,
}

impl RequestLog {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn push(&mut self, phase: &str, method: &str, url: &str, status: u16, note: &str) {
        self.entries.push(LogEntry {
            phase: phase.to_string(),
            method: method.to_string(),
            url: url.to_string(),
            status,
            note: note.to_string(),
        });
    }
}

async fn save_log(log: &RequestLog, output: &PathBuf) -> Result<(), WmuError> {
    tokio::fs::create_dir_all(output).await?;
    let path = output.join("request-log.txt");
    let mut content = String::from("WMU Guest Auth - Request Log\n");
    content.push_str(&"=".repeat(60));
    content.push('\n');
    content.push('\n');

    for (i, entry) in log.entries.iter().enumerate() {
        content.push_str(&format!(
            "[{:2}] {:<7} {} {} → {} {}\n",
            i + 1,
            entry.phase,
            entry.method,
            entry.url,
            entry.status,
            entry.note
        ));
    }

    tokio::fs::write(&path, &content).await?;
    Ok(())
}

fn print_step(msg: &str) {
    println!("  {} {}", "→".cyan(), msg);
}

fn print_phase(n: u8, name: &str) {
    println!("{}", format!("┌─ Phase {n}: {name}").bold().cyan());
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
