use url::Url;

use crate::error::WmuError;

#[derive(Debug)]
pub struct WlcParams {
    pub switch_url: Url,
    pub ap_mac: String,
    pub wlan: String,
    pub status_code: u8,
    pub redirect_url: String,
    pub client_mac: String,
    pub redirect: String,
}

impl WlcParams {
    pub fn from_portal_url(portal_url: &Url) -> Result<Self, WmuError> {
        let pairs: Vec<(String, String)> = portal_url.query_pairs().into_owned().collect();
        let find = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());

        let switch_url = find("switch_url").ok_or(WmuError::MissingParam {
            param: "switch_url",
        })?;

        // statusCode absent = normal initial redirect. Default 0 (not 1) so
        // we don't trigger the "already logged in" skip path for URLs that
        // legitimately omit it. statusCode=1 is the WLC's explicit signal.
        let status_code = find("statusCode").and_then(|v| v.parse().ok()).unwrap_or(0);

        // redirect_url on POST is built by the browser as
        // "http://www.wmich.edu" + <rest after "redirect=">. We replicate
        // that by storing the raw `redirect` value here; auth.rs constructs
        // the final redirect_url field to match browser behavior.
        let redirect = find("redirect").unwrap_or_default();

        Ok(Self {
            switch_url: Url::parse(&switch_url)?,
            ap_mac: find("ap_mac").unwrap_or_default(),
            wlan: find("wlan").unwrap_or_default(),
            status_code,
            redirect_url: String::new(),
            client_mac: find("client_mac").unwrap_or_default(),
            redirect,
        })
    }

    pub fn direct_default() -> Self {
        Self {
            switch_url: Url::parse("https://virtual.wireless.wmich.edu/login.html").unwrap(),
            ap_mac: String::new(),
            wlan: "WMU Guest".to_string(),
            status_code: 0,
            redirect_url: String::new(),
            client_mac: String::new(),
            redirect: String::new(),
        }
    }
}

#[derive(Debug)]
pub struct PortalPage {
    pub html: String,
    pub params: WlcParams,
    pub asset_urls: Vec<AssetRef>,
}

#[derive(Debug, Clone)]
pub struct AssetRef {
    pub url: Url,
    pub kind: AssetKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    JavaScript,
    Css,
    Image,
}

impl std::fmt::Display for AssetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JavaScript => f.write_str("js"),
            Self::Css => f.write_str("css"),
            Self::Image => f.write_str("img"),
        }
    }
}

pub async fn fetch_portal(portal_url: &Url) -> Result<PortalPage, WmuError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    let html = client.get(portal_url.as_str()).send().await?.text().await?;
    let params = WlcParams::from_portal_url(portal_url)?;
    let asset_urls = extract_assets(portal_url, &html);

    Ok(PortalPage {
        html,
        params,
        asset_urls,
    })
}

fn extract_assets(base_url: &Url, html: &str) -> Vec<AssetRef> {
    let document = scraper::Html::parse_document(html);
    let mut assets = Vec::new();

    let script_sel = scraper::Selector::parse("script[src]").unwrap();
    for el in document.select(&script_sel) {
        if let Some(src) = el.value().attr("src") {
            if let Ok(url) = base_url.join(src) {
                assets.push(AssetRef {
                    url,
                    kind: AssetKind::JavaScript,
                });
            }
        }
    }

    let link_sel = scraper::Selector::parse("link[rel='stylesheet']").unwrap();
    for el in document.select(&link_sel) {
        if let Some(href) = el.value().attr("href") {
            if let Ok(url) = base_url.join(href) {
                assets.push(AssetRef {
                    url,
                    kind: AssetKind::Css,
                });
            }
        }
    }

    let img_sel = scraper::Selector::parse("img[src]").unwrap();
    for el in document.select(&img_sel) {
        if let Some(src) = el.value().attr("src") {
            if let Ok(url) = base_url.join(src) {
                assets.push(AssetRef {
                    url,
                    kind: AssetKind::Image,
                });
            }
        }
    }

    assets
}

pub async fn fetch_wlc_page(switch_url: &Url) -> Result<(String, Vec<AssetRef>), WmuError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()?;

    let html = client.get(switch_url.as_str()).send().await?.text().await?;
    let assets = extract_assets(switch_url, &html);
    Ok((html, assets))
}
