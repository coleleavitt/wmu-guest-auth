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
    eprintln!(
        "wmu-guest-auth: authenticate target={} ap_mac={} client_mac={} statusCode={}",
        params.switch_url, params.ap_mac, params.client_mac, params.status_code,
    );
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .cookie_store(true)
        .user_agent(BROWSER_USER_AGENT)
        .build()?;

    // Cisco WLC consent flow requires a GET to login.html before POST to
    // establish session state on the controller. Without it the POST is
    // accepted (HTTP 200) but the client MAC is never moved to the
    // authenticated list. Errors here are non-fatal; proceed to POST.
    eprintln!(
        "wmu-guest-auth: pre-GET {} (WLC session bootstrap)",
        params.switch_url
    );
    match client.get(params.switch_url.as_str()).send().await {
        Ok(r) => eprintln!("wmu-guest-auth: pre-GET → HTTP {}", r.status().as_u16()),
        Err(e) => eprintln!("wmu-guest-auth: pre-GET failed ({e}), continuing"),
    }

    // Replicate the portal's submitAction() exactly: if a `redirect=` param
    // was in the source URL, final redirect_url = "http://www.wmich.edu"
    // concatenated with the raw rest (yes, Cisco/WMU's JS really does build
    // a malformed URL this way — the WLC ignores the value). If no redirect
    // param, leave blank just like the browser.
    let mut redirect_url = if params.redirect.is_empty() {
        String::new()
    } else {
        format!("http://www.wmich.edu{}", params.redirect)
    };
    if redirect_url.len() > 255 {
        redirect_url.truncate(255);
    }

    // Match the WLC's full form schema (7 fields per wlc-login.html) so any
    // firmware variant that validates field presence is satisfied.
    let form = [
        ("buttonClicked", "4"),
        ("err_flag", "0"),
        ("err_msg", ""),
        ("info_flag", "0"),
        ("info_msg", ""),
        ("redirect_url", redirect_url.as_str()),
        ("network_name", "Guest Network"),
    ];

    let post_once = |client: &reqwest::Client| {
        let client = client.clone();
        let switch_url = params.switch_url.clone();
        let form = form.clone();
        async move {
            let resp = client
                .post(switch_url.as_str())
                .header("Referer", POLICY_PAGE_URL)
                .header("Origin", POLICY_ORIGIN)
                .form(&form)
                .send()
                .await?;
            let status = resp.status().as_u16();
            let body = resp.text().await?;
            Ok::<_, reqwest::Error>((status, body))
        }
    };

    eprintln!(
        "wmu-guest-auth: POST {} (buttonClicked=4)",
        params.switch_url
    );
    let post_start = std::time::Instant::now();
    let (mut status, mut body) = post_once(&client).await?;
    eprintln!(
        "wmu-guest-auth: POST → HTTP {status} in {}ms (body={} bytes)",
        post_start.elapsed().as_millis(),
        body.len()
    );

    // err_flag=1 in response body = WLC rejected with "prior attempt
    // failed". Cisco's own loginscript.js submitAction() handles this by
    // re-POSTing with err_flag=0 and the same redirect_url. Replicate that
    // one-shot retry; repeated err_flag=1 means genuine failure.
    let body_lower = body.to_lowercase();
    let err_flag_set = body_lower
        .contains("name=\"err_flag\" size=\"16\" maxlength=\"15\" value=\"1\"")
        || body_lower.contains("err_flag\" value=\"1\"");
    if err_flag_set {
        eprintln!("wmu-guest-auth: response has err_flag=1, retrying POST");
        let retry = post_once(&client).await?;
        status = retry.0;
        body = retry.1;
    }

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
        .danger_accept_invalid_hostnames(true)
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
