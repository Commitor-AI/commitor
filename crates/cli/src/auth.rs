//! Login/session management for the Commitor CLI.
//!
//! `commitor login` opens the web app and auto-issues an API key via the
//! browser redirect — no manual key creation or copying required. The CLI
//! stores the key locally and attaches it as `Authorization: Bearer <key>`
//! on every request to the Commitor backend.
//!
//! The backend base URL defaults to a local development server and can
//! be overridden with `COMMITOR_API_URL` (no recompile needed when the
//! hosted API ships).

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};

use crate::admin;
use crate::config;

/// Where users manage their keys — shown in error messages.
pub const DASHBOARD_URL: &str = "https://commitor-web.vercel.app/dashboard";

/// Production-live frontend used by the browser-based login flow.
const FRONTEND_URL: &str = "https://commitor-web.vercel.app";

/// Local port the CLI listens on for the browser redirect after login.
const CALLBACK_PORT: u16 = 18745;

/// Seconds before an API call gives up.
const API_TIMEOUT_SECS: u64 = 15;

#[derive(Serialize, Deserialize)]
struct StoredCredentials {
    api_key: String,
}

/// Response of `GET /auth/me`.
#[derive(Debug, Deserialize)]
struct MeResponse {
    email: String,
    plan: Option<String>,
    /// Whether the backend recognizes this account as an admin. Older
    /// backends omit the field; treat its absence as `false`.
    admin: Option<bool>,
}

/// Backend base URL, e.g. `http://localhost:8000` (no trailing slash).
fn api_base_url() -> String {
    config::api_base_url()
}

fn credentials_path() -> Result<PathBuf> {
    let home = home_dir().context("could not locate your home directory")?;
    Ok(home.join(".commitor").join("credentials.toml"))
}

#[cfg(unix)]
fn home_dir() -> Result<PathBuf> {
    let home =
        std::env::var("HOME").context("HOME is not set — where should credentials be stored?")?;
    Ok(PathBuf::from(home))
}

#[cfg(windows)]
fn home_dir() -> Result<PathBuf> {
    let profile = std::env::var("USERPROFILE")
        .context("USERPROFILE is not set — where should credentials be stored?")?;
    Ok(PathBuf::from(profile))
}

/// The exact message shown when a command needs credentials and none
/// are stored.
fn not_logged_in_error() -> anyhow::Error {
    anyhow::anyhow!(
        "Not logged in. Run `commitor login` (opens your browser) or \
         `commitor login --key <your-key>` — get a key at {DASHBOARD_URL}"
    )
}

/// Plan string behind an already-loaded key, defaulting to `"free"`
/// when the backend omits the field (older backends). Network/404
/// failures surface as errors so callers can decide how to degrade.
pub fn plan_for_key(api_key: &str) -> Result<String> {
    let me = fetch_me(api_key)?;
    Ok(me.plan.unwrap_or_else(|| "free".to_string()))
}

/// Does the backend recognize `api_key`'s account as an admin? This is
/// the *source of truth* for admin status — the local admin file is only
/// ever written after this returns `true`, so being "admin" means the
/// backend verified you, not that a file was dropped on disk.
pub fn backend_is_admin(api_key: &str) -> Result<bool> {
    let me = fetch_me(api_key)?;
    Ok(me.admin.unwrap_or(false))
}

/// Plans that unlock Commitor's pro features.
pub const PRO_PLANS: &[&str] = &["pro", "team", "enterprise"];

/// Does `plan` grant access to pro features? `admin` is also a pro plan,
/// because the admin role is meant to unlock everything.
pub fn has_pro_access(plan: &str) -> bool {
    let plan = plan.trim().to_ascii_lowercase();
    plan == "admin" || PRO_PLANS.contains(&plan.as_str())
}

/// The effective plan for the stored key, overriding the backend plan
/// with `"admin"` whenever the (backend-verified) local admin role is
/// granted. This is the value callers should consult when deciding
/// feature access: a verified admin always reaches every pro feature,
/// even on a `free` account.
pub fn effective_plan(api_key: &str) -> Result<String> {
    if admin::is_admin() {
        return Ok("admin".to_string());
    }
    plan_for_key(api_key)
}

/// Load the stored API key.
///
/// This is what other (future) commands call to authenticate: pair it
/// with [`authorized`] to attach the bearer header to any request.
pub fn load_api_key() -> Result<String> {
    let path = credentials_path()?;
    let raw = fs::read_to_string(&path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => not_logged_in_error(),
        _ => anyhow::Error::new(err)
            .context(format!("failed to read {}", path.display())),
    })?;

    let creds: StoredCredentials = toml::from_str(&raw)
        .with_context(|| format!("{} is not valid TOML", path.display()))?;

    let key = creds.api_key.trim().to_string();
    if key.is_empty() {
        bail!("{} has an empty api_key — log in again to fix it", path.display());
    }
    Ok(key)
}

/// Attach `Authorization: Bearer <stored-key>` to a request.
///
/// Implemented for both the blocking and the async reqwest request
/// builders, so every authenticated command can route its requests
/// through this one helper.
pub trait WithBearer {
    fn set_bearer(self, api_key: &str) -> Self;
}

impl WithBearer for RequestBuilder {
    fn set_bearer(self, api_key: &str) -> Self {
        self.bearer_auth(api_key)
    }
}

impl WithBearer for reqwest::RequestBuilder {
    fn set_bearer(self, api_key: &str) -> Self {
        self.bearer_auth(api_key)
    }
}

/// Attach an already-loaded API key as `Authorization: Bearer` to a
/// request builder (blocking or async).
pub fn with_key<B: WithBearer>(request: B, api_key: &str) -> B {
    request.set_bearer(api_key)
}

/// Load the stored key and attach it as `Authorization: Bearer` to a
/// request builder.
///
/// This is the entry point future authenticated commands should use.
/// Fails with the standard not-logged-in message when no credentials
/// are stored.
#[allow(dead_code)]
pub fn authorized<B: WithBearer>(request: B) -> Result<B> {
    let api_key = load_api_key()?;
    Ok(with_key(request, &api_key))
}

/// Validate `api_key` against `GET /auth/me`, returning the account.
fn fetch_me(api_key: &str) -> Result<MeResponse> {
    let url = format!("{}/auth/me", api_base_url());
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(API_TIMEOUT_SECS))
        .build()
        .context("failed to set up HTTP client")?;

    let response = match with_key(client.get(&url), api_key).send() {
        Ok(response) => response,
        Err(err) => bail!(
            "Couldn't reach the Commitor API at {url} ({}). \
             Is your server running? Set COMMITOR_API_URL if it lives elsewhere.",
            root_cause(&err)
        ),
    };

    check_auth_status(response, &url)
}

/// The deepest error in a chain, e.g. `Connection refused (os error
/// 111)` — the part a human can actually act on.
pub fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut latest = err.to_string();
    let mut current = err.source();
    while let Some(source) = current {
        latest = source.to_string();
        current = source.source();
    }
    latest
}

/// Map non-success statuses onto friendly messages shared by
/// `login --key` and `whoami`.
fn check_auth_status(response: Response, url: &str) -> Result<MeResponse> {
    let status = response.status();
    match status.as_u16() {
        200 => response
            .json()
            .with_context(|| format!("{url} returned a malformed /auth/me response")),
        401 | 403 => bail!(
            "That API key was rejected by the backend (HTTP {status}).\n\
             Double-check it, or generate a new one at {DASHBOARD_URL}"
        ),
        404 => bail!(
            "{url} has no /auth/me endpoint — is COMMITOR_API_URL pointing at the right server?"
        ),
        _ => {
            let body = response.text().unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            if snippet.is_empty() {
                bail!("Commitor API returned HTTP {status} for {url}");
            }
            bail!("Commitor API returned HTTP {status} for {url}\nServer said: {snippet}");
        }
    }
}

/// Validate the key against the backend and store it on success.
pub fn login(raw_key: &str) -> Result<()> {
    let api_key = raw_key.trim().to_string();
    if api_key.is_empty() {
        bail!("The --key value is empty — pass the key from {DASHBOARD_URL}");
    }

    println!("Validating key against {}…", api_base_url());
    let me = fetch_me(&api_key)?;

    save_credentials(&api_key)?;

    let plan = me.plan.unwrap_or_else(|| "free".to_string());
    println!("Logged in as {} ({plan} plan).", me.email);
    let path = credentials_path()
        .unwrap_or_else(|_| PathBuf::from("~/.commitor/credentials.toml"));
    println!("Credentials stored at {}", path.display());
    Ok(())
}

fn save_credentials(api_key: &str) -> Result<()> {
    let path = credentials_path()?;
    let dir = path
        .parent()
        .context("credentials path has no parent directory")?
        .to_path_buf();

    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to restrict permissions on {}", dir.display()))?;
    }

    let body = toml::to_string_pretty(&StoredCredentials {
        api_key: api_key.to_string(),
    })
    .context("failed to serialize credentials")?;

    fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to restrict permissions on {}", path.display()))?;
    }

    Ok(())
}

/// Browser-based login for users who aren't already authenticated.
///
/// Opens the web app's login page and starts a tiny local HTTP server
/// that captures the API key when the frontend redirects back to
/// `http://127.0.0.1:<CALLBACK_PORT>/callback?key=...`. If the frontend
/// doesn't support the redirect (or the browser flow is interrupted), the
/// user can paste an API key from the dashboard instead.
pub fn login_interactive() -> Result<()> {
    let redirect = format!("http://127.0.0.1:{CALLBACK_PORT}/callback");
    let login_url = format!("{FRONTEND_URL}/login?redirect={redirect}");
    println!("Opening your browser to sign in: {login_url}");
    let _ = webbrowser::open(&login_url);

    let (tx, rx) = mpsc::channel::<LoginSource>();

    let mut have_server = false;
    if let Ok(listener) = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)) {
        have_server = true;
        let server_tx = tx.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                if let Ok(mut stream) = stream {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let req = String::from_utf8_lossy(&buf);
                    if let Some(key) = extract_key(&req) {
                        let _ = write_connected(&mut stream);
                        let _ = server_tx.send(LoginSource::Callback(key));
                        break;
                    } else {
                        let _ = write_error(
                            &mut stream,
                            "No API key was returned. Run `commitor login` again.",
                        );
                    }
                }
            }
        });
    } else {
        println!("(Couldn't start a local callback server; you'll need to paste your key.)");
    }

    if have_server {
        println!("If your browser redirects back here automatically, you're all set.");
    }
    println!("Otherwise, paste your API key from {DASHBOARD_URL} and press Enter:");

    let stdin_tx = tx.clone();
    thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            let key = line.trim().to_string();
            if !key.is_empty() {
                let _ = stdin_tx.send(LoginSource::Manual(key));
            }
        }
    });

    match rx.recv() {
        Ok(source) => {
            let key = match source {
                LoginSource::Callback(k) | LoginSource::Manual(k) => k,
            };
            login(&key)
        }
        Err(_) => bail!("Login was cancelled."),
    }
}

/// How the API key reached `login_interactive`: via the browser redirect
/// or pasted by the user.
enum LoginSource {
    Callback(String),
    Manual(String),
}

/// Pull the `key` query parameter out of a raw HTTP request line.
fn extract_key(req: &str) -> Option<String> {
    let line = req.lines().next()?;
    let path = line.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("key=") {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Minimal percent-decoder for the key (handles `%XX` and `+`).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8 as char);
                    i += 3;
                } else {
                    out.push('%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Render the success/error "connected" page shown in the browser after
/// the `commitor login` redirect. Self-contained (inline CSS + SVG) and
/// styled to match the Commitor web app: dark canvas, lime brand accent,
/// animated draw-in check with a breathing glow — no emoji.
fn page_html(
    title: &str,
    subtitle: &str,
    caption: &str,
    accent: &str,
    glow: &str,
    svg: &str,
) -> String {
    const TEMPLATE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Commitor</title>
<style>
  :root{
    --canvas:#050506; --surface:#0c0d10; --edge:rgba(255,255,255,.08);
    --ink:#f4f6f5; --dim:#9ba1a6; --dimmer:#62676d;
    --brand:ACCENT; --brand-bright:ACCENT; --glow:GLOW;
  }
  *{box-sizing:border-box}
  html,body{height:100%}
  body{
    margin:0; background:
      radial-gradient(1100px 560px at 50% -12%, GLOW, transparent 60%),
      var(--canvas);
    color:var(--ink);
    font-family:Inter,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;
    display:flex; align-items:center; justify-content:center; padding:24px;
    -webkit-font-smoothing:antialiased;
  }
  .card{
    width:100%; max-width:384px; text-align:center; padding:44px 34px 30px;
    background:
      linear-gradient(180deg, rgba(255,255,255,.025), rgba(255,255,255,0)),
      var(--surface);
    border:1px solid var(--edge); border-radius:18px;
    box-shadow:0 1px 0 rgba(255,255,255,.05) inset, 0 24px 70px rgba(0,0,0,.55);
    animation:fade-up .5s cubic-bezier(.2,.7,.2,1) both;
  }
  .badge{position:relative; width:72px; height:72px; margin:0 auto 24px}
  .badge::after{
    content:""; position:absolute; inset:-16px; border-radius:999px;
    background:radial-gradient(circle, var(--glow), transparent 70%);
    animation:pulse 2.6s ease-in-out infinite;
  }
  .check{position:relative; width:72px; height:72px; display:block}
  .ring{
    fill:none; stroke:var(--brand); stroke-width:2.5;
    stroke-dasharray:151; stroke-dashoffset:151;
    animation:draw-ring .6s cubic-bezier(.6,.1,.3,1) forwards;
  }
  .tick{
    fill:none; stroke:var(--brand-bright); stroke-width:3.5;
    stroke-linecap:round; stroke-linejoin:round;
    stroke-dasharray:42; stroke-dashoffset:42;
    filter:drop-shadow(0 0 6px var(--glow));
    animation:draw-tick .45s .5s cubic-bezier(.6,.1,.3,1) forwards;
  }
  h1{margin:0 0 9px; font-size:19px; font-weight:600; letter-spacing:-.01em}
  p{margin:0; color:var(--dim); font-size:14px; line-height:1.55}
  .caption{
    margin-top:24px;
    font-family:"JetBrains Mono",ui-monospace,SFMono-Regular,Menlo,monospace;
    font-size:11px; letter-spacing:.06em; text-transform:uppercase; color:var(--dimmer);
  }
  .caption .dot{
    display:inline-block; width:6px; height:6px; border-radius:50%;
    background:var(--brand); margin-right:8px; vertical-align:middle;
    animation:blink 1.5s steps(1) infinite;
  }
  @keyframes fade-up{from{opacity:0; transform:translateY(10px) scale(.99)} to{opacity:1; transform:none}}
  @keyframes draw-ring{to{stroke-dashoffset:0}}
  @keyframes draw-tick{to{stroke-dashoffset:0}}
  @keyframes pulse{0%,100%{opacity:.45; transform:scale(.96)} 50%{opacity:.85; transform:scale(1.06)}}
  @keyframes blink{0%,100%{opacity:1} 50%{opacity:.25}}
</style>
</head>
<body>
  <main class="card">
    <div class="badge">
      <svg class="check" viewBox="0 0 52 52" aria-hidden="true">SVG</svg>
    </div>
    <h1>TITLE</h1>
    <p>SUBTITLE</p>
    <div class="caption"><span class="dot"></span>CAPTION</div>
  </main>
</body>
</html>"#;
    TEMPLATE
        .replace("ACCENT", accent)
        .replace("GLOW", glow)
        .replace("SVG", svg)
        .replace("CAPTION", caption)
        .replace("SUBTITLE", subtitle)
        .replace("TITLE", title)
}

/// Write a self-contained HTML document back to the browser redirect.
fn write_http(stream: &mut impl Write, html: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\
         \r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    stream.write_all(response.as_bytes())
}

/// Page shown when the browser redirect delivered a valid API key:
/// the user is connected and can return to the terminal.
fn write_connected(stream: &mut impl Write) -> std::io::Result<()> {
    const SVG: &str =
        r#"<circle class="ring" cx="26" cy="26" r="24"/><path class="tick" d="M15 27 l7 7 l15 -16"/>"#;
    let html = page_html(
        "You're connected to Commitor",
        "You can close this tab and return to your terminal.",
        "session established",
        "#b8f23d",
        "rgba(198,255,0,.35)",
        SVG,
    );
    write_http(stream, &html)
}

/// Page shown when the redirect did not carry a key (rare — e.g. the user
/// navigated to the callback manually). Keeps the same look, in the
/// critical (red) accent.
fn write_error(stream: &mut impl Write, msg: &str) -> std::io::Result<()> {
    const SVG: &str =
        r#"<circle class="ring" cx="26" cy="26" r="24"/><path class="tick" d="M17 17 l18 18 M35 17 l-18 18"/>"#;
    let html = page_html(
        "Couldn't connect",
        msg,
        "connection failed",
        "#f43f5e",
        "rgba(244,63,94,.30)",
        SVG,
    );
    write_http(stream, &html)
}

/// Remove the stored credentials file, if any.
pub fn logout() -> Result<()> {
    let path = credentials_path()?;
    match fs::remove_file(&path) {
        Ok(()) => println!("Logged out. Deleted {}.", path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("You weren't logged in — nothing to remove.")
        }
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("failed to delete {}", path.display())))
        }
    }
    Ok(())
}

/// Print the email + plan behind the stored key.
pub fn whoami() -> Result<()> {
    let api_key = load_api_key()?;
    let me = fetch_me(&api_key)?;
    let raw_plan = me.plan.unwrap_or_else(|| "free".to_string());
    let backend_admin = me.admin.unwrap_or(false);
    let local_admin = admin::is_admin();

    if local_admin {
        if backend_admin {
            println!("{} — {} plan", me.email, raw_plan);
            println!("admin: verified by backend — all pro features unlocked.");
        } else {
            println!("{} — {} plan", me.email, raw_plan);
            println!(
                "admin: a local grant is cached, but the backend no longer \
                 reports this account as admin. Run `commitor admin revoke`."
            );
        }
    } else if backend_admin {
        println!("{} — {} plan", me.email, raw_plan);
        println!("admin: verified by backend, but not activated locally. Run `commitor gimme admin`.");
    } else {
        let plan = effective_plan(&api_key)?;
        let pro = if has_pro_access(&plan) { "yes" } else { "no" };
        println!("{} — {} plan", me.email, plan);
        println!("pro features: {pro}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_page_renders_without_placeholders() {
        const SVG: &str =
            r#"<circle class="ring" cx="26" cy="26" r="24"/><path class="tick" d="M15 27 l7 7 l15 -16"/>"#;
        let html = page_html(
            "You're connected to Commitor",
            "You can close this tab and return to your terminal.",
            "session established",
            "#b8f23d",
            "rgba(198,255,0,.35)",
            SVG,
        );
        for token in ["ACCENT", "GLOW", "SVG", "TITLE", "SUBTITLE", "CAPTION"] {
            assert!(
                !html.contains(token),
                "placeholder {token} leaked into rendered page"
            );
        }
        assert!(html.contains("stroke-dashoffset"));
        assert!(html.contains("<h1>You're connected to Commitor</h1>"));
        assert!(html.contains("<p>You can close this tab and return to your terminal.</p>"));
        assert!(html.contains("<div class=\"caption\"><span class=\"dot\"></span>session established</div>"));
        std::fs::write("/tmp/commitor-connected.html", &html).ok();
    }
}
