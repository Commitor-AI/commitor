//! Shared runtime configuration (API location).

/// Backend the CLI talks to. Local development default; flip to the
/// hosted URL (e.g. `https://api.commitor.dev`) before shipping a
/// release that expects production.
pub const DEFAULT_API_URL: &str = "https://commitor-api.vercel.app";

/// Backend base URL, e.g. `http://localhost:8000` (no trailing slash).
///
/// `COMMITOR_API_URL` overrides the default — mainly for pointing a
/// dev build at a local backend.
pub fn api_base_url() -> String {
    std::env::var("COMMITOR_API_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}
