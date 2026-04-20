use thiserror::Error;

#[derive(Debug, Error)]
pub enum WmuError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("failed to parse URL: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("dns resolution failed: {0}")]
    Dns(#[from] hickory_resolver::ResolveError),

    #[error("missing query parameter: {param}")]
    MissingParam { param: &'static str },

    #[error("no switch_url found in portal page")]
    NoSwitchUrl,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("auth failed: WLC returned status {status}")]
    AuthFailed { status: u16 },

    #[error("wifi error: {msg}")]
    Wifi { msg: String },
}
