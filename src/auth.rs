use url::Url;

use crate::error::WmuError;
use crate::portal::WlcParams;

#[derive(Debug)]
pub struct AuthResult {
    pub success: bool,
    pub status: u16,
    pub logout_url: Option<String>,
    pub response_body: String,
}

const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

const POLICY_PAGE_URL: &str = "https://legacy.wmich.edu/oit/guest/wmu-guest-policy.html";
const POLICY_ORIGIN: &str = "https://legacy.wmich.edu";

pub async fn authenticate(params: &WlcParams) -> Result<AuthResult, WmuError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .cookie_store(true)
        .user_agent(BROWSER_USER_AGENT)
        .build()?;

    // Cisco WLC consent flow requires a GET to login.html before POST to
    // establish session state on the controller. Without it the POST is
    // accepted (HTTP 200) but the client MAC is never moved to the
    // authenticated list. Errors here are non-fatal; proceed to POST.
    let _ = client.get(params.switch_url.as_str()).send().await;

    let mut redirect_url = params.redirect_url.clone();
    if redirect_url.is_empty() {
        redirect_url = "http://www.wmich.edu".to_string();
    }
    if redirect_url.len() > 255 {
        redirect_url.truncate(255);
    }

    let form = [
        ("buttonClicked", "4"),
        ("redirect_url", redirect_url.as_str()),
        ("err_flag", "0"),
    ];

    let resp = client
        .post(params.switch_url.as_str())
        .header("Referer", POLICY_PAGE_URL)
        .header("Origin", POLICY_ORIGIN)
        .form(&form)
        .send()
        .await?;

    let status = resp.status().as_u16();
    let body = resp.text().await?;

    // Strict success detection: require an explicit success marker AND
    // confirm the response is NOT still the login form. The previous
    // heuristic of matching "userStatus" or "logout.html" alone was wrong:
    // both strings appear in the login form itself (hidden input and
    // footer link), so a rejected auth returning the form was marked
    // successful. True success is confirmed later via connectivity re-probe.
    let body_lower = body.to_lowercase();
    let still_on_login_form = body_lower.contains("buttonclicked") && body_lower.contains("<form");
    let has_success_marker = body_lower.contains("login successful")
        || body_lower.contains("you are now connected")
        || body_lower.contains("logout.html");
    let success = has_success_marker && !still_on_login_form;

    let logout_url = if success {
        let base = Url::parse(params.switch_url.as_str())?;
        Some(base.join("/logout.html")?.to_string())
    } else {
        None
    };

    Ok(AuthResult {
        success,
        status,
        logout_url,
        response_body: body,
    })
}

pub async fn deauthenticate(switch_url: &Url) -> Result<AuthResult, WmuError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .user_agent(BROWSER_USER_AGENT)
        .build()?;

    let logout_url = switch_url.join("/logout.html")?;

    let resp = client
        .post(logout_url.as_str())
        .form(&[("userStatus", "1"), ("err_flag", "0"), ("err_msg", "")])
        .send()
        .await?;

    let status = resp.status().as_u16();
    let body = resp.text().await?;

    Ok(AuthResult {
        success: true,
        status,
        logout_url: None,
        response_body: body,
    })
}
