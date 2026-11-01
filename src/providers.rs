//! Where each provider keeps its credentials, which endpoint reports usage, and how to
//! read the answer. Credential files are only written to store a Claude token that this
//! program renewed itself, because nothing else renews it while Claude Code is not running.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tinyjson::JsonValue;

use crate::json::{self, get, num, text};
use crate::model::{FetchError, Provider, Usage, Window};
use crate::timefmt::parse_iso8601;

#[path = "providers/web.rs"]
pub mod web;

const USER_AGENT: &str = concat!("iusage/", env!("CARGO_PKG_VERSION"));
/// Used when no installed Claude Code version can be found.
const FALLBACK_CLAUDE_CODE_VERSION: &str = "2.1.284";

/// Where Claude Code trades a refresh token for a new access token.
const CLAUDE_TOKEN_HOST: &str = "platform.claude.com";
const CLAUDE_TOKEN_PATH: &str = "/v1/oauth/token";
/// Claude Code's own OAuth client, which the stored token was issued to.
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Digest of the refresh token the server last turned down, 0 for none. The same dead
/// token is not offered again until the credential file holds a different one.
static REJECTED_REFRESH: AtomicU64 = AtomicU64::new(0);

/// Where the providers' own software keeps things on this machine.
pub struct Env {
    pub home: PathBuf,
    /// Claude Code's configuration folder, which holds `.credentials.json`.
    pub claude_dir: PathBuf,
    /// Folders in which the Claude desktop app may keep one sub-folder per Claude Code
    /// version. Which one exists depends on how the desktop app was installed.
    pub claude_code_dirs: Vec<PathBuf>,
}

/// An HTTPS request, described without any platform types: a GET, or a POST when it has
/// a body.
pub struct Request {
    pub host: &'static str,
    pub path: String,
    pub user_agent: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<String>,
}

pub struct Response {
    pub status: u16,
    pub body: String,
    /// The Retry-After header in seconds, if present.
    pub retry_after: Option<u32>,
}

/// Platform HTTP client.
pub type Http = fn(&Request) -> Result<Response, String>;

/// Select locally before any I/O. A failure never triggers another channel.
pub fn fetch_selected(provider: Provider, env: &Env, now: i64, http: Http,
    session: Option<&web::Session>, before_network: &mut dyn FnMut() -> Result<(), FetchError>,
) -> Result<(Usage, crate::cache::Source), FetchError> {
    if provider == Provider::Claude {
        if let Some(session) = session {
            return web::fetch(session, http, before_network).map(|usage| (usage, crate::cache::Source::Web));
        }
    }
    let request = prepare(provider, env, now, http, before_network)?;
    before_network()?;
    send_request(provider, &request, http).map(|usage| (usage, if provider == Provider::Claude {
        crate::cache::Source::OAuth
    } else { crate::cache::Source::Codex }))
}

pub fn fetch(provider: Provider, env: &Env, now: i64, http: Http) -> Result<Usage, FetchError> {
    let request = prepare(provider, env, now, http, &mut || Ok(()))?;
    send_request(provider, &request, http)
}

/// The usage request for `provider`. An expired Claude token is renewed first;
/// `before_network` runs just before that renewal uses the network, and not at all when
/// nothing needs renewing or the renewal cannot be tried.
pub fn prepare(
    provider: Provider,
    env: &Env,
    now: i64,
    http: Http,
    before_network: &mut dyn FnMut() -> Result<(), FetchError>,
) -> Result<Request, FetchError> {
    match build_request(provider, env, now) {
        Err(FetchError::Expired) if provider == Provider::Claude => {
            refresh_claude(env, now, http, before_network)?;
            build_request(provider, env, now)
        }
        other => other,
    }
}

pub fn send_request(provider: Provider, request: &Request, http: Http) -> Result<Usage, FetchError> {
    let response = http(request).map_err(FetchError::Network)?;
    match response.status {
        200 => read_answer(provider, &response.body),
        401 | 403 => Err(FetchError::Unauthorized),
        429 => Err(FetchError::RateLimited(response.retry_after)),
        other => Err(FetchError::Http(other)),
    }
}

/// A successful usage answer, or `Unsubscribed` when the account has no paid quota to
/// meter. Codex names its plan. Claude does not, but an answer whose windows are all
/// empty, with no newer `limits` list either, leaves nothing to show; a body without the
/// window keys at all is still an unknown shape (`Parse`), not a free account.
pub fn read_answer(provider: Provider, body: &str) -> Result<Usage, FetchError> {
    let root = json::parse(body).ok_or(FetchError::Parse)?;
    if provider == Provider::Codex && get(&root, "plan_type").and_then(text).is_some_and(unpaid_plan) {
        return Err(FetchError::Unsubscribed);
    }
    let usage = parse_root(provider, &root)?;
    let limits = matches!(get(&root, "limits"), Some(JsonValue::Array(items)) if !items.is_empty());
    if provider == Provider::Claude && usage == Usage::default() && !limits {
        return Err(FetchError::Unsubscribed);
    }
    Ok(usage)
}

/// ChatGPT plans without a subscription. Free accounts still get a small monthly Codex
/// window, but nobody on one needs a meter polling it.
fn unpaid_plan(plan: &str) -> bool {
    let plan = plan.to_ascii_lowercase();
    plan.starts_with("free") || plan == "guest"
}

/// The windows alone, without the plan check of `read_answer`. The macOS core reads
/// answers through this.
#[allow(dead_code)]
pub fn parse(provider: Provider, body: &str) -> Result<Usage, FetchError> {
    parse_root(provider, &json::parse(body).ok_or(FetchError::Parse)?)
}

fn parse_root(provider: Provider, root: &JsonValue) -> Result<Usage, FetchError> {
    match provider {
        Provider::Claude => parse_claude(root),
        Provider::Codex => parse_codex(root),
    }
}

fn config_dir(env_var: &str, home: &Path, default: &str) -> PathBuf {
    match std::env::var_os(env_var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.join(default),
    }
}

/// Claude Code's configuration folder, which `CLAUDE_CONFIG_DIR` may relocate.
pub fn claude_dir(home: &Path) -> PathBuf {
    config_dir("CLAUDE_CONFIG_DIR", home, ".claude")
}

fn read_json(path: &Path) -> Result<JsonValue, FetchError> {
    let text = json::read_small_file(path).map_err(|_| FetchError::NoCredentials)?;
    json::parse(&text).ok_or(FetchError::NoCredentials)
}

/// The usage endpoint exists for Claude Code, and other tools that read it all identify
/// as Claude Code; callers that do not are believed to be rate limited sooner. That has
/// not been measured here. The request therefore names the Claude Code version that is
/// installed on this machine.
fn claude_user_agent(claude_code_dirs: &[PathBuf]) -> String {
    let version = installed_claude_code(claude_code_dirs).unwrap_or_else(|| FALLBACK_CLAUDE_CODE_VERSION.to_string());
    format!("claude-code/{version}")
}

/// The highest `major.minor.patch` among the folder names in `dirs`. Folders that do
/// not exist are skipped.
fn installed_claude_code(dirs: &[PathBuf]) -> Option<String> {
    let parse = |name: &str| -> Option<[u32; 3]> {
        let mut parts = name.split('.');
        let version = [parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?];
        parts.next().is_none().then_some(version)
    };
    dirs.iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| parse(entry.ok()?.file_name().to_str()?))
        .max()
        .map(|[major, minor, patch]| format!("{major}.{minor}.{patch}"))
}

pub fn build_request(provider: Provider, env: &Env, now: i64) -> Result<Request, FetchError> {
    let home = env.home.as_path();
    match provider {
        Provider::Claude => {
            let root = read_json(&claude_credentials(env))?;
            let oauth = get(&root, "claudeAiOauth").ok_or(FetchError::NoCredentials)?;
            let token = get(oauth, "accessToken").and_then(text).ok_or(FetchError::NoCredentials)?;
            // expiresAt is in milliseconds. An expired token is never sent; `prepare`
            // renews it first.
            if let Some(expires_ms) = get(oauth, "expiresAt").and_then(num) {
                if (expires_ms / 1000.0) as i64 <= now {
                    return Err(FetchError::Expired);
                }
            }
            Ok(Request {
                host: "api.anthropic.com",
                path: "/api/oauth/usage".into(),
                user_agent: claude_user_agent(&env.claude_code_dirs),
                headers: vec![
                    ("Authorization", format!("Bearer {token}")),
                    ("anthropic-beta", "oauth-2025-04-20".to_string()),
                    ("Accept", "application/json".to_string()),
                ],
                body: None,
            })
        }
        Provider::Codex => {
            let root = read_json(&codex_auth(home))?;
            let tokens = get(&root, "tokens").ok_or(FetchError::NoCredentials)?;
            let token = get(tokens, "access_token").and_then(text).ok_or(FetchError::NoCredentials)?;
            let mut headers = vec![
                ("Authorization", format!("Bearer {token}")),
                ("Accept", "application/json".to_string()),
            ];
            if let Some(account) = get(tokens, "account_id").and_then(text) {
                headers.push(("ChatGPT-Account-Id", account.to_string()));
            }
            Ok(Request {
                host: "chatgpt.com",
                path: "/backend-api/wham/usage".into(),
                user_agent: USER_AGENT.to_string(),
                headers,
                body: None,
            })
        }
    }
}

fn claude_credentials(env: &Env) -> PathBuf {
    env.claude_dir.join(".credentials.json")
}

fn codex_auth(home: &Path) -> PathBuf {
    config_dir("CODEX_HOME", home, ".codex").join("auth.json")
}

/// Unix seconds at which the provider's own software last wrote its sign-in: Claude Code
/// its `.credentials.json`, Codex its `auth.json`. They do so on a new login and when they
/// renew tokens, which is when a new subscription would reach them.
pub fn signed_in_at(provider: Provider, env: &Env) -> Option<i64> {
    let file = match provider {
        Provider::Claude => claude_credentials(env),
        Provider::Codex => codex_auth(&env.home),
    };
    let modified = std::fs::metadata(file).ok()?.modified().ok()?;
    modified.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
}

/// What the token endpoint answered to a successful renewal.
struct Renewal {
    access_token: String,
    /// Absent when the server keeps the refresh token as it was.
    refresh_token: Option<String>,
    expires_in_secs: i64,
    refresh_expires_in_secs: Option<i64>,
}

/// Tokens end up in a header and a file; anything but plain visible characters is
/// refused rather than escaped.
fn token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= 4096 && token.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn parse_renewal(body: &str) -> Result<Renewal, FetchError> {
    let root = json::parse(body).ok_or(FetchError::Parse)?;
    let token = |key: &str| get(&root, key).and_then(text).filter(|t| token_ok(t)).map(String::from);
    let seconds = |key: &str, most: f64| get(&root, key).and_then(num).filter(|n| (1.0..=most).contains(n)).map(|n| n as i64);
    let refresh_token = match get(&root, "refresh_token") {
        None | Some(JsonValue::Null) => None,
        Some(_) => Some(token("refresh_token").ok_or(FetchError::Parse)?),
    };
    Ok(Renewal {
        access_token: token("access_token").ok_or(FetchError::Parse)?,
        refresh_token,
        expires_in_secs: seconds("expires_in", 366.0 * 86_400.0).ok_or(FetchError::Parse)?,
        refresh_expires_in_secs: seconds("refresh_token_expires_in", 3660.0 * 86_400.0),
    })
}

fn digest(token: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish().max(1)
}

/// The request Claude Code itself sends to renew a token: the stored refresh token, the
/// client and the scopes it was issued with.
fn refresh_request(user_agent: String, oauth: &JsonValue, refresh_token: &str) -> Request {
    let client_id = get(oauth, "clientId").and_then(text).filter(|id| token_ok(id)).unwrap_or(CLAUDE_CLIENT_ID);
    let scope = match get(oauth, "scopes") {
        Some(JsonValue::Array(items)) => items.iter().filter_map(text).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    };
    let mut body = format!(
        r#"{{"grant_type":"refresh_token","refresh_token":{},"client_id":{}"#,
        json_string(refresh_token),
        json_string(client_id)
    );
    if !scope.is_empty() {
        body.push_str(&format!(r#","scope":{}"#, json_string(&scope)));
    }
    body.push('}');
    Request {
        host: CLAUDE_TOKEN_HOST,
        path: CLAUDE_TOKEN_PATH.into(),
        user_agent,
        headers: vec![("Content-Type", "application/json".to_string()), ("Accept", "application/json".to_string())],
        body: Some(body),
    }
}

/// A renewal that passed every check that can be made without the network.
pub struct RefreshPlan {
    pub request: Request,
    /// The refresh token the request spends; what the store must still hold afterwards.
    refresh: String,
}

/// Decides, from the stored credentials `stored`, whether Claude's access token needs
/// renewing and whether it is safe to try. `Ok(None)`: it is not expired, someone else
/// renewed it since the caller looked. Everything that can fail locally is checked here,
/// before the server is asked to retire the old refresh token: `can_store` says whether
/// the store takes a write, and the layout of `stored` must be patchable.
pub fn plan_claude_refresh(
    stored: &str,
    now: i64,
    user_agent: String,
    can_store: &mut dyn FnMut() -> bool,
) -> Result<Option<RefreshPlan>, FetchError> {
    let root = json::parse(stored).ok_or(FetchError::NoCredentials)?;
    let oauth = get(&root, "claudeAiOauth").ok_or(FetchError::NoCredentials)?;
    let past = |key: &str| get(oauth, key).and_then(num).is_some_and(|ms| (ms / 1000.0) as i64 <= now);
    if !past("expiresAt") {
        return Ok(None);
    }
    let refresh = get(oauth, "refreshToken").and_then(text).filter(|t| token_ok(t)).ok_or(FetchError::Expired)?;
    if past("refreshTokenExpiresAt") {
        return Err(FetchError::Expired);
    }
    if REJECTED_REFRESH.load(Ordering::Relaxed) == digest(refresh) {
        // Nothing is sent, so this is a local verdict: the store is looked at again soon
        // and a new sign-in is picked up as soon as it is stored.
        return Err(FetchError::Expired);
    }
    // Prove the store can be written, and that the layout can be patched, before the
    // server is asked to retire the old refresh token. The stand-ins differ from what
    // is stored, so a member that cannot take effect (a repeated key, say) shows up.
    let access = get(oauth, "accessToken").and_then(text).unwrap_or_default();
    let stand_in = [
        ("accessToken", json_string(&format!("{access}~"))),
        ("refreshToken", json_string(&format!("{refresh}~"))),
        ("expiresAt", "-1".to_string()),
    ];
    if !can_store() || patch_oauth(stored, &root, &stand_in).is_none() {
        return Err(FetchError::CannotSave);
    }
    Ok(Some(RefreshPlan { request: refresh_request(user_agent, oauth, refresh), refresh: refresh.to_string() }))
}

/// The server's answer to `plan`'s request. `Ok(Some(text))` is the credentials to store
/// in place of what is there; `Ok(None)` means leave the store alone, because another
/// process renewed in the meantime and its tokens are the ones to keep. `current` reads
/// the store again and is only called once the answer is a usable renewal.
pub fn finish_claude_refresh(
    plan: &RefreshPlan,
    response: &Response,
    now: i64,
    current: &mut dyn FnMut() -> Result<String, FetchError>,
) -> Result<Option<String>, FetchError> {
    match response.status {
        200 => {}
        400 | 401 if json::parse(&response.body)
            .and_then(|root| get(&root, "error").and_then(text).map(|s| s.eq_ignore_ascii_case("invalid_grant")))
            .unwrap_or(false) => {
            REJECTED_REFRESH.store(digest(&plan.refresh), Ordering::Relaxed);
            return Err(FetchError::Unauthorized);
        }
        429 => return Err(FetchError::RateLimited(response.retry_after)),
        other => return Err(FetchError::RefreshTemporary(other)),
    }
    let renewal = parse_renewal(&response.body)?;

    // If the refresh token stored now is no longer the one used, another process renewed
    // in the meantime and its tokens are the ones to keep.
    let file = current()?;
    let root = json::parse(&file).ok_or(FetchError::NoCredentials)?;
    let oauth = get(&root, "claudeAiOauth").ok_or(FetchError::NoCredentials)?;
    if get(oauth, "refreshToken").and_then(text) != Some(plan.refresh.as_str()) {
        return Ok(None);
    }
    let from_now_ms = |secs: i64| (now.saturating_add(secs)).saturating_mul(1000).to_string();
    let mut updates = vec![
        ("accessToken", json_string(&renewal.access_token)),
        ("refreshToken", json_string(renewal.refresh_token.as_deref().unwrap_or(&plan.refresh))),
        ("expiresAt", from_now_ms(renewal.expires_in_secs)),
    ];
    if let (Some(secs), Some(_)) = (renewal.refresh_expires_in_secs, get(oauth, "refreshTokenExpiresAt")) {
        updates.push(("refreshTokenExpiresAt", from_now_ms(secs)));
    }
    patch_oauth(&file, &root, &updates).map(Some).ok_or(FetchError::CannotSave)
}

/// Renews Claude's expired access token with the refresh token stored beside it and
/// writes the new tokens back. Neither the Claude desktop app (it keeps its own sign-in)
/// nor anything else does that while Claude Code is not running, so without this the
/// meter would stop every eight hours.
///
/// A server that rotates refresh tokens makes a lost write costly, so everything that can
/// fail locally is checked before the network is used. Another process that renewed the
/// token in the meantime always wins: its file is left alone.
pub fn refresh_claude(
    env: &Env,
    now: i64,
    http: Http,
    before_network: &mut dyn FnMut() -> Result<(), FetchError>,
) -> Result<(), FetchError> {
    let path = claude_credentials(env);
    let read = || json::read_small_file(&path).map_err(|_| FetchError::NoCredentials);
    let file = read()?;
    let user_agent = claude_user_agent(&env.claude_code_dirs);
    let Some(plan) = plan_claude_refresh(&file, now, user_agent, &mut || can_replace(&path))? else {
        return Ok(());
    };

    before_network()?;
    let response = http(&plan.request).map_err(FetchError::Network)?;
    let Some(updated) = finish_claude_refresh(&plan, &response, now, &mut || read())? else {
        return Ok(());
    };
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        if json::write_file(&path, &updated).is_ok() {
            return Ok(());
        }
    }
    Err(FetchError::CannotSave)
}

/// Whether the credential file can be replaced: it is not read-only and its folder
/// accepts new files.
fn can_replace(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if meta.permissions().readonly() {
        return false;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.probe", std::process::id()));
    let probe = path.with_file_name(name);
    let created = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    created
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `text` with the values of some members of its `claudeAiOauth` object replaced by the
/// given JSON, and every other byte as it was. `original` is `text` parsed. `None` unless
/// the result parses to exactly `original` with those members changed, so a surprising
/// file is left alone rather than rewritten into something else.
fn patch_oauth(text: &str, original: &JsonValue, updates: &[(&str, String)]) -> Option<String> {
    let start = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let top = object_members(text, skip_space(text.as_bytes(), start))?;
    let (oauth_start, _) = member(&top, "claudeAiOauth")?;
    let inner = object_members(text, oauth_start)?;
    let mut spans = Vec::new();
    for (key, value) in updates {
        let (from, to) = member(&inner, key)?;
        spans.push((from, to, value));
    }
    spans.sort_by_key(|span| std::cmp::Reverse(span.0));
    let mut patched = text.to_string();
    for (from, to, value) in spans {
        patched.replace_range(from..to, value);
    }

    let mut expected = original.clone();
    let JsonValue::Object(root) = &mut expected else { return None };
    let JsonValue::Object(oauth) = root.get_mut("claudeAiOauth")? else { return None };
    for (key, value) in updates {
        oauth.insert((*key).to_string(), json::parse(value)?);
    }
    (json::parse(&patched)? == expected).then_some(patched)
}

fn member(members: &[(&str, usize, usize)], key: &str) -> Option<(usize, usize)> {
    members.iter().find(|(name, _, _)| *name == key).map(|&(_, from, to)| (from, to))
}

fn skip_space(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(|b| b.is_ascii_whitespace()) {
        at += 1;
    }
    at
}

/// One past the closing quote of the string that starts at `at`.
fn string_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    loop {
        match *bytes.get(i)? {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

/// One past the end of the JSON value that starts at `at`.
fn value_end(bytes: &[u8], at: usize) -> Option<usize> {
    match *bytes.get(at)? {
        b'"' => string_end(bytes, at),
        b'{' | b'[' => {
            let (mut depth, mut i) = (0usize, at);
            loop {
                match *bytes.get(i)? {
                    b'"' => {
                        i = string_end(bytes, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        _ => {
            let mut i = at;
            while bytes.get(i).is_some_and(|b| !matches!(b, b',' | b'}' | b']') && !b.is_ascii_whitespace()) {
                i += 1;
            }
            (i > at).then_some(i)
        }
    }
}

/// The members of the object that opens at `open`: each name as written, and where its
/// value starts and ends.
fn object_members(text: &str, open: usize) -> Option<Vec<(&str, usize, usize)>> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut members = Vec::new();
    let mut i = skip_space(bytes, open + 1);
    if bytes.get(i) == Some(&b'}') {
        return Some(members);
    }
    loop {
        if bytes.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = string_end(bytes, i)?;
        let key = &text[i + 1..key_end - 1];
        i = skip_space(bytes, key_end);
        if bytes.get(i) != Some(&b':') {
            return None;
        }
        i = skip_space(bytes, i + 1);
        let end = value_end(bytes, i)?;
        members.push((key, i, end));
        i = skip_space(bytes, end);
        match bytes.get(i)? {
            b',' => i = skip_space(bytes, i + 1),
            b'}' => return Some(members),
            _ => return None,
        }
    }
}

fn parse_claude(root: &JsonValue) -> Result<Usage, FetchError> {
    if get(root, "five_hour").is_none() && get(root, "seven_day").is_none() {
        return Err(FetchError::Parse);
    }
    let window = |key: &str| -> Result<Option<Window>, FetchError> {
        let Some(w) = get(root, key).filter(|v| !matches!(v, JsonValue::Null)) else { return Ok(None) };
        let used_pct = percentage(w, "utilization")?;
        let resets_at = match get(w, "resets_at") {
            None | Some(JsonValue::Null) => None,
            Some(value) => Some(text(value).and_then(parse_iso8601).ok_or(FetchError::Parse)?),
        };
        Ok(Some(Window { used_pct, resets_at }))
    };
    Ok(Usage { five_hour: window("five_hour")?, seven_day: window("seven_day")? })
}

fn percentage(value: &JsonValue, key: &str) -> Result<f64, FetchError> {
    get(value, key).and_then(num).filter(|n| (0.0..=100.0).contains(n)).ok_or(FetchError::Parse)
}

fn parse_codex(root: &JsonValue) -> Result<Usage, FetchError> {
    let limits = get(root, "rate_limit").ok_or(FetchError::Parse)?;
    if get(limits, "primary_window").is_none() && get(limits, "secondary_window").is_none() {
        return Err(FetchError::Parse);
    }
    let mut usage = Usage::default();
    for (key, is_primary) in [("primary_window", true), ("secondary_window", false)] {
        let Some(w) = get(limits, key).filter(|v| !matches!(v, JsonValue::Null)) else { continue };
        let used_pct = percentage(w, "used_percent")?;
        let window = Window {
            used_pct,
            resets_at: match get(w, "reset_at") {
                None | Some(JsonValue::Null) => None,
                Some(value) => Some(num(value).filter(|n| (0.0..=253_402_300_799.0).contains(n)).ok_or(FetchError::Parse)? as i64),
            },
        };
        // Tell the windows apart by their length; fall back to position if it is missing.
        let short = match get(w, "limit_window_seconds").and_then(num) {
            Some(secs) if secs > 0.0 => secs <= 86_400.0,
            _ => is_primary,
        };
        if short {
            if usage.five_hour.is_some() { return Err(FetchError::Parse) }
            usage.five_hour = Some(window);
        } else {
            if usage.seven_day.is_some() { return Err(FetchError::Parse) }
            usage.seven_day = Some(window);
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from real responses captured in phase 0.
    const CLAUDE: &str = r#"{"five_hour":{"utilization":4.0,"resets_at":"2026-10-01T17:30:00.472092+00:00","limit_dollars":null},
        "seven_day":{"utilization":62.0,"resets_at":"2026-10-02T02:00:00.472116+00:00"},
        "seven_day_opus":null,"iguana_necktie":{"utilization":0.0,"resets_at":"2026-11-05T07:59:00+00:00"},
        "extra_usage":{"is_enabled":false,"monthly_limit":null}}"#;

    const CODEX: &str = r#"{"plan_type":"plus","rate_limit":{"allowed":true,"limit_reached":false,
        "primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_after_seconds":18000,"reset_at":1790876671},
        "secondary_window":{"used_percent":8,"limit_window_seconds":604800,"reset_after_seconds":250205,"reset_at":1791108875}},
        "code_review_rate_limit":null,"credits":{"has_credits":false}}"#;

    #[test]
    fn claude_requests_carry_the_installed_claude_code_version() {
        let dir = std::env::temp_dir().join(format!("iusage-cc-test-{}", std::process::id()));
        // The first folder never exists, as on a machine where the desktop app keeps
        // its data in the other location.
        let dirs = [dir.join("absent"), dir.join("present")];
        assert_eq!(claude_user_agent(&dirs), format!("claude-code/{FALLBACK_CLAUDE_CODE_VERSION}"));
        for name in ["2.1.99", "2.1.284", "2.0.300", "notes", "3.x.1", "2.1.284.1"] {
            std::fs::create_dir_all(dirs[1].join(name)).unwrap();
        }
        assert_eq!(claude_user_agent(&dirs), "claude-code/2.1.284");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_claude_usage() {
        let usage = parse(Provider::Claude, CLAUDE).unwrap();
        assert_eq!(usage.five_hour, Some(Window { used_pct: 4.0, resets_at: Some(1_790_875_800) }));
        assert_eq!(usage.seven_day, Some(Window { used_pct: 62.0, resets_at: Some(1_790_906_400) }));
    }

    #[test]
    fn claude_window_without_active_session() {
        let usage = parse(Provider::Claude, r#"{"five_hour":null,"seven_day":{"utilization":10,"resets_at":null}}"#).unwrap();
        assert_eq!(usage.five_hour, None);
        assert_eq!(usage.seven_day, Some(Window { used_pct: 10.0, resets_at: None }));
    }

    #[test]
    fn parses_codex_usage() {
        let usage = parse(Provider::Codex, CODEX).unwrap();
        assert_eq!(usage.five_hour, Some(Window { used_pct: 0.0, resets_at: Some(1_790_876_671) }));
        assert_eq!(usage.seven_day, Some(Window { used_pct: 8.0, resets_at: Some(1_791_108_875) }));
    }

    #[test]
    fn codex_windows_are_identified_by_length_not_position() {
        let swapped = r#"{"rate_limit":{
            "primary_window":{"used_percent":8,"limit_window_seconds":604800,"reset_at":2},
            "secondary_window":{"used_percent":3,"limit_window_seconds":18000,"reset_at":1}}}"#;
        let usage = parse(Provider::Codex, swapped).unwrap();
        assert_eq!(usage.five_hour.unwrap().used_pct, 3.0);
        assert_eq!(usage.seven_day.unwrap().used_pct, 8.0);
    }

    #[test]
    fn rejects_unexpected_bodies() {
        assert_eq!(parse(Provider::Claude, "<html>blocked</html>"), Err(FetchError::Parse));
        assert_eq!(parse(Provider::Claude, "[]"), Err(FetchError::Parse));
        assert_eq!(parse(Provider::Codex, r#"{"rate_limit":null}"#), Err(FetchError::Parse));
        assert_eq!(parse(Provider::Claude, &"[".repeat(200_000)), Err(FetchError::Parse));
        for body in [r#"{}"#, r#"{"error":"unavailable"}"#, r#"{"five_hour":{}}"#,
            r#"{"five_hour":{"utilization":-1}}"#, r#"{"five_hour":{"utilization":101}}"#,
            r#"{"five_hour":{"utilization":1,"resets_at":"tomorrow"}}"#] {
            assert_eq!(parse(Provider::Claude, body), Err(FetchError::Parse), "{body}");
        }
        for body in [r#"{"rate_limit":{}}"#, r#"{"rate_limit":{"primary_window":{}}}"#,
            r#"{"rate_limit":{"primary_window":{"used_percent":10,"reset_at":"bad"}}}"#] {
            assert_eq!(parse(Provider::Codex, body), Err(FetchError::Parse), "{body}");
        }
    }

    #[test]
    fn http_errors_keep_the_retry_information_without_live_requests() {
        let request =
            Request { host: "unused.invalid", path: "/".into(), user_agent: String::new(), headers: Vec::new(), body: None };
        let limited: Http = |_| Ok(Response { status: 429, body: String::new(), retry_after: Some(600) });
        assert_eq!(send_request(Provider::Claude, &request, limited), Err(FetchError::RateLimited(Some(600))));
        let denied: Http = |_| Ok(Response { status: 401, body: String::new(), retry_after: None });
        assert_eq!(send_request(Provider::Codex, &request, denied), Err(FetchError::Unauthorized));
        let broken: Http = |_| Ok(Response { status: 200, body: "{}".into(), retry_after: None });
        assert_eq!(send_request(Provider::Claude, &request, broken), Err(FetchError::Parse));
    }

    #[test]
    fn a_free_codex_account_is_told_apart_from_a_paid_one() {
        let request =
            Request { host: "unused.invalid", path: "/".into(), user_agent: String::new(), headers: Vec::new(), body: None };
        // Shaped like a free account's answer: one month-long window, no five-hour one.
        let free: Http = |_| Ok(Response { status: 200, retry_after: None, body: r#"{"plan_type":"free","rate_limit":{
            "primary_window":{"used_percent":0,"limit_window_seconds":2592000,"reset_at":1793500000},"secondary_window":null}}"#.into() });
        assert_eq!(send_request(Provider::Codex, &request, free), Err(FetchError::Unsubscribed));
        for plan in ["guest", "free_workspace", "FREE"] {
            assert!(unpaid_plan(plan), "{plan}");
        }
        for plan in ["plus", "pro", "team", "business", "enterprise", "edu", "go", ""] {
            assert!(!unpaid_plan(plan), "{plan}");
        }
        let paid: Http = |_| Ok(Response { status: 200, body: CODEX.into(), retry_after: None });
        assert!(send_request(Provider::Codex, &request, paid).is_ok());
        // Without the field nothing changes, and the bare parser (shared with macOS) still reads the windows.
        let unknown: Http = |_| Ok(Response { status: 200, body: CODEX.replace(r#""plan_type":"plus","#, ""), retry_after: None });
        assert!(send_request(Provider::Codex, &request, unknown).is_ok());
        assert!(parse(Provider::Codex, &CODEX.replace("plus", "free")).is_ok());
    }

    #[test]
    fn a_claude_answer_without_any_window_has_nothing_to_meter() {
        for body in [r#"{"five_hour":null,"seven_day":null,"seven_day_opus":null,"extra_usage":{"is_enabled":false}}"#,
            r#"{"five_hour":null,"limits":[]}"#, r#"{"seven_day":null,"limits":null}"#] {
            assert_eq!(read_answer(Provider::Claude, body), Err(FetchError::Unsubscribed), "{body}");
        }
        // One window is enough to keep metering, quota moved to a `limits` list is a format
        // change rather than a free account, and a body without window keys is unknown.
        assert!(read_answer(Provider::Claude, CLAUDE).is_ok());
        assert!(read_answer(Provider::Claude, r#"{"five_hour":null,"seven_day":{"utilization":0,"resets_at":null}}"#).is_ok());
        assert_eq!(read_answer(Provider::Claude, r#"{"five_hour":null,"seven_day":null,"limits":[{"kind":"session","percent":5}]}"#),
            Ok(Usage::default()));
        for body in ["{}", r#"{"limits":[]}"#, "<html>blocked</html>"] {
            assert_eq!(read_answer(Provider::Claude, body), Err(FetchError::Parse), "{body}");
        }
        // The bare parser shared with macOS is unchanged.
        assert_eq!(parse(Provider::Claude, r#"{"five_hour":null,"seven_day":null}"#), Ok(Usage::default()));
        // Codex is judged by its plan alone.
        let empty = r#"{"plan_type":"plus","rate_limit":{"primary_window":null,"secondary_window":null}}"#;
        assert_eq!(read_answer(Provider::Codex, empty), parse(Provider::Codex, empty));
    }

    #[test]
    fn sign_in_time_comes_from_each_providers_own_file() {
        let dir = workdir("sign-in-files");
        let env = env_in(&dir);
        assert_eq!(signed_in_at(Provider::Claude, &env), None);
        assert_eq!(signed_in_at(Provider::Codex, &env), None);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let recent = |at: Option<i64>| at.is_some_and(|at| (now - 5..=now + 5).contains(&at));
        store(&dir, "{}");
        assert!(recent(signed_in_at(Provider::Claude, &env)));
        assert_eq!(signed_in_at(Provider::Codex, &env), None);
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        std::fs::write(dir.join(".codex").join("auth.json"), "{}").unwrap();
        assert!(recent(signed_in_at(Provider::Codex, &env)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // --- Renewing an expired Claude token. A fake network stands in for the servers: no
    // test here sends a real request or touches the real credential file.

    use std::cell::RefCell;

    struct Call {
        host: &'static str,
        path: String,
        user_agent: String,
        headers: Vec<(&'static str, String)>,
        body: Option<String>,
    }

    type Reply = Result<(u16, String, Option<u32>), String>;

    #[derive(Default)]
    struct Network {
        calls: Vec<Call>,
        token_reply: Option<Reply>,
        /// Runs while the token request is "in flight".
        during_token_request: Option<fn()>,
    }

    thread_local! {
        static NETWORK: RefCell<Network> = RefCell::new(Network::default());
    }

    fn fake_http(request: &Request) -> Result<Response, String> {
        let (reply, hook) = NETWORK.with(|network| {
            let mut network = network.borrow_mut();
            network.calls.push(Call {
                host: request.host,
                path: request.path.clone(),
                user_agent: request.user_agent.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            });
            if request.host == CLAUDE_TOKEN_HOST {
                (network.token_reply.clone(), network.during_token_request.take())
            } else {
                (Some(Ok((200, CLAUDE.to_string(), None))), None)
            }
        });
        if let Some(hook) = hook {
            hook();
        }
        let (status, body, retry_after) = reply.expect("no reply set up")?;
        Ok(Response { status, body, retry_after })
    }

    fn serve(reply: Reply) {
        NETWORK.with(|network| *network.borrow_mut() = Network { token_reply: Some(reply), ..Network::default() });
    }

    fn hosts() -> Vec<&'static str> {
        NETWORK.with(|network| network.borrow().calls.iter().map(|call| call.host).collect())
    }

    const NOW: i64 = 1_790_900_000;
    const RENEWED: &str = r#"{"access_token":"new-access","refresh_token":"refresh-2","expires_in":28800,"refresh_token_expires_in":2592000,"scope":"user:inference"}"#;

    /// Credentials as Claude Code writes them, plus things this program must not disturb:
    /// another member holding the same key names, brackets inside strings, odd spacing.
    fn credentials(refresh: &str, expires_at: i64) -> String {
        r#"{
  "mcpOAuth": { "accessToken": "keep-me", "list": [1, {"a": "}"}] },
  "claudeAiOauth": {
    "accessToken": "old-access",
    "refreshToken": "REFRESH",
    "expiresAt": EXPIRES,
    "refreshTokenExpiresAt": 1791900000000,
    "scopes": ["user:inference", "user:profile"],
    "subscriptionType":   "pro"
  },
  "tail": null
}
"#
        .replace("REFRESH", refresh)
        .replace("EXPIRES", &(expires_at * 1000).to_string())
    }

    fn workdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iusage-refresh-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn env_in(dir: &Path) -> Env {
        Env { home: dir.to_path_buf(), claude_dir: dir.to_path_buf(), claude_code_dirs: Vec::new() }
    }

    fn store(dir: &Path, text: &str) {
        std::fs::write(dir.join(".credentials.json"), text).unwrap();
    }

    fn stored(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(".credentials.json")).unwrap()
    }

    #[test]
    fn oversized_credentials_are_rejected_before_network_or_rewrite() {
        let dir = workdir("oversized");
        let env = env_in(&dir);
        // Valid JSON and credentials, but an accidentally bloated file.
        let oversized = format!("{}{}", credentials("refresh-large", NOW - 60), " ".repeat(1024 * 1024));
        store(&dir, &oversized);
        assert!(matches!(build_request(Provider::Claude, &env, NOW), Err(FetchError::NoCredentials)));
        assert!(matches!(refresh_claude(&env, NOW, fake_http, &mut || panic!("must not reach network")), Err(FetchError::NoCredentials)));
        assert_eq!(stored(&dir), oversized);
        let codex = dir.join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(codex.join("auth.json"), &oversized).unwrap();
        assert!(matches!(build_request(Provider::Codex, &env, NOW), Err(FetchError::NoCredentials)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_expired_token_is_renewed_and_only_its_own_values_change() {
        let dir = workdir("renew");
        let before = credentials("refresh-1", NOW - 60);
        store(&dir, &before);
        serve(Ok((200, RENEWED.to_string(), None)));

        let usage = fetch(Provider::Claude, &env_in(&dir), NOW, fake_http).unwrap();
        assert_eq!(usage.seven_day.unwrap().used_pct, 62.0);

        let expected = before
            .replace("old-access", "new-access")
            .replace("refresh-1", "refresh-2")
            .replace(&((NOW - 60) * 1000).to_string(), &((NOW + 28_800) * 1000).to_string())
            .replace("1791900000000", &((NOW + 2_592_000) * 1000).to_string());
        assert_eq!(stored(&dir), expected, "every other byte stays as written");

        assert_eq!(hosts(), [CLAUDE_TOKEN_HOST, "api.anthropic.com"]);
        NETWORK.with(|network| {
            let network = network.borrow();
            let renewal = &network.calls[0];
            assert_eq!(renewal.path, "/v1/oauth/token");
            assert!(renewal.user_agent.starts_with("claude-code/"));
            assert!(renewal.headers.contains(&("Content-Type", "application/json".to_string())));
            let body = json::parse(renewal.body.as_ref().unwrap()).unwrap();
            assert_eq!(get(&body, "grant_type").and_then(text), Some("refresh_token"));
            assert_eq!(get(&body, "refresh_token").and_then(text), Some("refresh-1"));
            assert_eq!(get(&body, "client_id").and_then(text), Some(CLAUDE_CLIENT_ID));
            assert_eq!(get(&body, "scope").and_then(text), Some("user:inference user:profile"));
            let usage = &network.calls[1];
            assert!(usage.body.is_none());
            assert!(usage.headers.contains(&("Authorization", "Bearer new-access".to_string())));
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_network_is_announced_before_it_is_used_and_only_then() {
        let dir = workdir("hook");
        store(&dir, &credentials("refresh-hook", NOW - 60));
        serve(Ok((200, RENEWED.to_string(), None)));
        let mut announced = 0;
        let mut hook = || -> Result<(), FetchError> {
            announced += 1;
            assert!(hosts().is_empty(), "announced before any request");
            Ok(())
        };
        prepare(Provider::Claude, &env_in(&dir), NOW, fake_http, &mut hook).unwrap();
        assert_eq!(announced, 1);

        // Nothing to renew, or nothing to renew it with: the hook is not called.
        let mut hook = || -> Result<(), FetchError> { panic!("must not announce") };
        store(&dir, &credentials("refresh-hook", NOW + 3600));
        prepare(Provider::Claude, &env_in(&dir), NOW, fake_http, &mut hook).unwrap();
        store(&dir, &credentials("refresh-hook", NOW - 60).replace("\"refreshToken\": \"refresh-hook\",", ""));
        assert_eq!(prepare(Provider::Claude, &env_in(&dir), NOW, fake_http, &mut hook).err(), Some(FetchError::Expired));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_valid_token_is_never_renewed() {
        let dir = workdir("valid");
        let before = credentials("refresh-valid", NOW + 600);
        store(&dir, &before);
        serve(Ok((200, RENEWED.to_string(), None)));
        fetch(Provider::Claude, &env_in(&dir), NOW, fake_http).unwrap();
        assert_eq!(hosts(), ["api.anthropic.com"]);
        assert_eq!(stored(&dir), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_a_usable_refresh_token_the_token_stays_expired_and_nothing_is_sent() {
        let dir = workdir("norefresh");
        serve(Ok((200, RENEWED.to_string(), None)));
        let none = credentials("x", NOW - 60).replace("\"refreshToken\": \"x\",", "");
        let empty = credentials("", NOW - 60);
        let spent = credentials("refresh-spent", NOW - 60).replace("1791900000000", &((NOW - 1) * 1000).to_string());
        for file in [none, empty, spent] {
            store(&dir, &file);
            assert_eq!(fetch(Provider::Claude, &env_in(&dir), NOW, fake_http), Err(FetchError::Expired));
            assert_eq!(stored(&dir), file);
        }
        assert!(hosts().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_refresh_token_is_not_offered_again_until_the_file_changes() {
        let dir = workdir("refused");
        let before = credentials("refresh-refused", NOW - 60);
        store(&dir, &before);
        serve(Ok((400, r#"{"error":"invalid_grant"}"#.to_string(), None)));
        // The refusal itself is a server answer and gets the usual wait; asking again
        // with the same token is then a local verdict.
        assert_eq!(fetch(Provider::Claude, &env_in(&dir), NOW, fake_http), Err(FetchError::Unauthorized));
        for _ in 0..3 {
            assert_eq!(fetch(Provider::Claude, &env_in(&dir), NOW, fake_http), Err(FetchError::Expired));
        }
        assert_eq!(hosts(), [CLAUDE_TOKEN_HOST], "asked once");
        assert_eq!(stored(&dir), before);

        // Signing in again stores a different refresh token, which is tried.
        store(&dir, &credentials("refresh-fresh-login", NOW - 60));
        serve(Ok((200, RENEWED.to_string(), None)));
        assert!(fetch(Provider::Claude, &env_in(&dir), NOW, fake_http).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn other_failures_of_the_renewal_leave_the_file_alone_and_are_reported_as_what_they_are() {
        let dir = workdir("failures");
        let before = credentials("refresh-failures", NOW - 60);
        store(&dir, &before);
        let attempt = |reply: Reply| {
            serve(reply);
            fetch(Provider::Claude, &env_in(&dir), NOW, fake_http)
        };
        assert_eq!(attempt(Ok((429, String::new(), Some(120)))), Err(FetchError::RateLimited(Some(120))));
        assert_eq!(attempt(Ok((503, String::new(), None))), Err(FetchError::RefreshTemporary(503)));
        assert_eq!(attempt(Err("receive timeout".into())), Err(FetchError::Network("receive timeout".into())));
        for body in [
            "", "{}", "[]", r#"{"access_token":"a"}"#,
            r#"{"access_token":"","expires_in":10}"#, r#"{"access_token":"a b","expires_in":10}"#,
            r#"{"access_token":"a\nb","expires_in":10}"#, r#"{"access_token":"a","expires_in":0}"#,
            r#"{"access_token":"a","expires_in":-5}"#, r#"{"access_token":"a","expires_in":"10"}"#,
            r#"{"access_token":"a","expires_in":1e12}"#, r#"{"access_token":"a","expires_in":10,"refresh_token":5}"#,
            r#"{"access_token":"a","expires_in":10,"refresh_token":""}"#,
        ] {
            assert_eq!(attempt(Ok((200, body.to_string(), None))), Err(FetchError::Parse), "{body}");
        }
        assert_eq!(stored(&dir), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn temporary_refresh_errors_do_not_poison_the_same_token() {
        let dir = workdir("transient-refresh");
        let before = credentials("refresh-transient", NOW - 60);
        store(&dir, &before);
        for (status, body) in [(403, r#"{"error":"invalid_grant"}"#), (400, "{}"), (401, "not json"), (400, r#"{"error":"temporarily_unavailable"}"#)] {
            serve(Ok((status, body.into(), None)));
            assert_eq!(fetch(Provider::Claude, &env_in(&dir), NOW, fake_http), Err(FetchError::RefreshTemporary(status)));
            assert_eq!(hosts(), [CLAUDE_TOKEN_HOST]);
            assert_eq!(stored(&dir), before);
        }
        serve(Ok((200, RENEWED.into(), None)));
        assert!(fetch(Provider::Claude, &env_in(&dir), NOW + 300, fake_http).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_response_without_a_new_refresh_token_keeps_the_old_one() {
        let dir = workdir("keep-refresh");
        let before = credentials("refresh-kept", NOW - 60);
        store(&dir, &before);
        serve(Ok((200, r#"{"access_token":"new-access","expires_in":3600}"#.to_string(), None)));
        fetch(Provider::Claude, &env_in(&dir), NOW, fake_http).unwrap();
        let expected = before
            .replace("old-access", "new-access")
            .replace(&((NOW - 60) * 1000).to_string(), &((NOW + 3600) * 1000).to_string());
        assert_eq!(stored(&dir), expected, "refreshToken and refreshTokenExpiresAt are unchanged");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tokens_with_quotes_and_backslashes_are_stored_faithfully() {
        let dir = workdir("escapes");
        store(&dir, &credentials("refresh-escapes", NOW - 60));
        serve(Ok((200, r#"{"access_token":"a\"b\\c","refresh_token":"r\\\"s","expires_in":60}"#.to_string(), None)));
        fetch(Provider::Claude, &env_in(&dir), NOW, fake_http).unwrap();
        let root = json::parse(&stored(&dir)).unwrap();
        let oauth = get(&root, "claudeAiOauth").unwrap();
        assert_eq!(get(oauth, "accessToken").and_then(text), Some(r#"a"b\c"#));
        assert_eq!(get(oauth, "refreshToken").and_then(text), Some(r#"r\"s"#));
        assert_eq!(get(get(&root, "mcpOAuth").unwrap(), "accessToken").and_then(text), Some("keep-me"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_renewed_by_someone_else_meanwhile_is_not_overwritten() {
        let dir = workdir("race");
        store(&dir, &credentials("refresh-race", NOW - 60));
        serve(Ok((200, RENEWED.to_string(), None)));
        // While the request is in flight Claude Code renews the token itself.
        NETWORK.with(|network| network.borrow_mut().during_token_request = Some(|| {
            let dir = std::env::temp_dir().join(format!("iusage-refresh-{}-race", std::process::id()));
            store(&dir, &credentials("refresh-theirs", NOW + 28_000).replace("old-access", "theirs-access"));
        }));
        fetch(Provider::Claude, &env_in(&dir), NOW, fake_http).unwrap();
        assert_eq!(stored(&dir), credentials("refresh-theirs", NOW + 28_000).replace("old-access", "theirs-access"));
        NETWORK.with(|network| {
            let usage = &network.borrow().calls[1];
            assert!(usage.headers.contains(&("Authorization", "Bearer theirs-access".to_string())));
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_cannot_be_replaced_is_never_worth_a_renewal() {
        let dir = workdir("readonly");
        let before = credentials("refresh-readonly", NOW - 60);
        store(&dir, &before);
        let path = dir.join(".credentials.json");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions.clone()).unwrap();
        serve(Ok((200, RENEWED.to_string(), None)));
        assert_eq!(fetch(Provider::Claude, &env_in(&dir), NOW, fake_http), Err(FetchError::CannotSave));
        assert!(hosts().is_empty(), "the server was not asked to retire the old refresh token");
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();

        // A file whose layout cannot be patched safely is treated the same way.
        store(&dir, r#"{"claudeAiOauth":{"accessToken":"a","accessToken":"b","refreshToken":"r","expiresAt":1000}}"#);
        assert_eq!(fetch(Provider::Claude, &env_in(&dir), NOW, fake_http), Err(FetchError::CannotSave));
        assert!(hosts().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_client_id_in_the_file_is_the_one_used_and_no_scopes_means_none_sent() {
        let dir = workdir("client");
        let file = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":5,"clientId":"custom-client"}}"#;
        store(&dir, file);
        let root = json::parse(file).unwrap();
        let request = refresh_request("ua".to_string(), get(&root, "claudeAiOauth").unwrap(), "r");
        assert_eq!(request.body.as_deref(), Some(r#"{"grant_type":"refresh_token","refresh_token":"r","client_id":"custom-client"}"#));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn patching_keeps_layout_and_refuses_what_it_cannot_place() {
        let patch = |text: &str, updates: &[(&str, String)]| {
            patch_oauth(text, &json::parse(text).unwrap(), updates)
        };
        let new = [("accessToken", "\"n\"".to_string()), ("expiresAt", "99".to_string())];
        let compact = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1}}"#;
        assert_eq!(patch(compact, &new).unwrap(), r#"{"claudeAiOauth":{"accessToken":"n","refreshToken":"r","expiresAt":99}}"#);
        let with_bom = format!("\u{feff}{compact}");
        assert_eq!(patch(&with_bom, &new).unwrap(), format!("\u{feff}{}", patch(compact, &new).unwrap()));
        // Only the member of the right object counts, wherever it comes in the file.
        let other_first = r#"{"x":{"claudeAiOauth":{"accessToken":"no","expiresAt":2}},"claudeAiOauth":{"accessToken":"a","expiresAt":1}}"#;
        assert_eq!(patch(other_first, &new).unwrap().matches("\"n\"").count(), 1);
        assert!(patch(other_first, &new).unwrap().contains(r#""x":{"claudeAiOauth":{"accessToken":"no""#));
        // Missing member, missing object, wrong shapes.
        assert!(patch(r#"{"claudeAiOauth":{"accessToken":"a"}}"#, &new).is_none());
        assert!(patch(r#"{"claudeOther":{}}"#, &new).is_none());
        assert!(patch(r#"{"claudeAiOauth":[]}"#, &new).is_none());
        assert!(patch(r#"[1]"#, &new).is_none());
        assert!(patch(compact, &[("accessToken", "not json".to_string())]).is_none());
    }
}
