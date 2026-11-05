//! Thin C ABI over the existing shared core. No HTTP or credentials cross this API.
#![allow(dead_code)]
#[path = "../../../src/model.rs"] mod model;
#[path = "../../../src/json.rs"] mod json;
#[path = "../../../src/providers.rs"] mod providers;
#[path = "../../../src/timefmt.rs"] mod timefmt;
#[path = "../../../src/schedule.rs"] mod schedule;
#[path = "../../../src/alert.rs"] mod alert;
#[path = "../../../src/cache.rs"] mod cache;
#[path = "../../../src/settings.rs"] mod settings;
#[path = "../../../src/view.rs"] mod view;
#[path = "../../../src/panel.rs"] mod panel;

#[repr(C)]
#[derive(Default, Debug)]
pub struct MacUsage {
    pub five_hour: f64,
    pub seven_day: f64,
    pub five_reset: i64,
    pub seven_reset: i64,
    pub present: u32,
}

/// Returns 0 on success, 1 on invalid input, 2 on a malformed response.
/// On failure, output is cleared. Missing windows/reset times are represented by
/// bits 1/2 and 4/8 in `present`, not by fabricated zero-percent observations.
///
/// # Safety
/// Non-null pointers must reference readable `len` bytes and writable MacUsage.
#[no_mangle]
pub unsafe extern "C" fn iusage_parse(provider: u32, bytes: *const u8, len: usize, out: *mut MacUsage) -> i32 {
    if out.is_null() { return 1; }
    *out = MacUsage::default();
    if bytes.is_null() || len == 0 || len > 1_048_576 { return 1; }
    let provider = match provider { 0 => model::Provider::Claude, 1 => model::Provider::Codex, _ => return 1 };
    let Ok(body) = std::str::from_utf8(std::slice::from_raw_parts(bytes, len)) else { return 1; };
    let Ok(usage) = providers::parse(provider, body) else { return 2; };
    if let Some(window) = usage.five_hour {
        (*out).five_hour = window.used_pct;
        (*out).present |= 1;
        if let Some(reset) = window.resets_at { (*out).five_reset = reset; (*out).present |= 4; }
    }
    if let Some(window) = usage.seven_day {
        (*out).seven_day = window.used_pct;
        (*out).present |= 2;
        if let Some(reset) = window.resets_at { (*out).seven_reset = reset; (*out).present |= 8; }
    }
    0
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct MacGate {
    pub last_attempt: i64,
    pub next_poll: i64,
    pub blocked_until: i64,
    pub backoff: i64,
}
impl From<MacGate> for schedule::State {
    fn from(s: MacGate) -> Self { Self { last_attempt: s.last_attempt, next_poll: s.next_poll, blocked_until: s.blocked_until, backoff: s.backoff } }
}
impl From<schedule::State> for MacGate {
    fn from(s: schedule::State) -> Self { Self { last_attempt: s.last_attempt, next_poll: s.next_poll, blocked_until: s.blocked_until, backoff: s.backoff } }
}

/// # Safety
/// `gate` must point to a readable MacGate. Null gates are denied.
#[no_mangle]
pub unsafe extern "C" fn iusage_can_request(gate: *const MacGate, now: i64) -> bool {
    !gate.is_null() && schedule::State::from(*gate).can_request(now)
}

/// Reserve an attempt before I/O. Caller must persist the updated gate before
/// sending; this function itself does not write to disk or perform network I/O.
/// # Safety
/// `gate` must point to writable MacGate storage.
#[no_mangle]
pub unsafe extern "C" fn iusage_start(gate: *mut MacGate, now: i64) -> bool {
    if gate.is_null() || !iusage_can_request(gate, now) { return false; }
    let mut state = schedule::State::from(*gate);
    state.started(now);
    *gate = state.into();
    true
}

/// Complete an attempt: status 200 succeeds; 0 is a network failure.
/// Other values are HTTP status codes. Retry-After is interpreted only for 429.
/// # Safety
/// `gate` must point to writable MacGate storage.
#[no_mangle]
pub unsafe extern "C" fn iusage_complete(gate: *mut MacGate, now: i64, status: u16, retry_after: u32, poll_secs: u32) -> bool {
    if gate.is_null() { return false; }
    let mut state = schedule::State::from(*gate);
    if status == 200 {
        state.succeeded(now, poll_secs.max(30));
    } else {
        let error = match status {
            0 => model::FetchError::Network(String::new()),
            401 | 403 => model::FetchError::Unauthorized,
            429 => model::FetchError::RateLimited(Some(retry_after)),
            _ => model::FetchError::Http(status),
        };
        state.failed(&error, now);
    }
    *gate = state.into();
    true
}

/// The shortest time between two requests to one service, and the default refresh
/// period: the shared Windows value, so the two platforms cannot drift apart.
#[no_mangle]
pub extern "C" fn iusage_min_gap_secs() -> i64 {
    schedule::MIN_GAP_SECS
}

/// Reminder decision for one fresh reading, using the shared `alert` rules.
///
/// `announced` holds the reminder points already announced for the 5-hour and 7-day
/// window (0..=3) and is updated in place; `previous_reset` is each window's reset time
/// when it was last checked (0 = none). `usage` is the new reading, laid out as
/// `iusage_parse` leaves it. The lines to show are written to `out`, separated by `\n`.
///
/// Returns 0 on success (possibly with no lines), 1 on invalid arguments or a buffer
/// that is too small; `announced` is left untouched in that case.
/// # Safety
/// Non-null pointers must reference readable/writable memory of the stated sizes
/// (`announced`: 2 bytes, `previous_reset`: 2 values).
#[no_mangle]
pub unsafe extern "C" fn iusage_alert_check(
    usage: *const MacUsage, announced: *mut u8, previous_reset: *const i64, now: i64,
    out: *mut u8, capacity: usize, out_len: *mut usize,
) -> i32 {
    if usage.is_null() || announced.is_null() || previous_reset.is_null() || out_len.is_null() { return 1; }
    *out_len = 0;
    let current = usage_of(&*usage);
    let previous_reset = std::slice::from_raw_parts(previous_reset, 2);
    let before = |reset: i64| Some(model::Window { used_pct: 0., resets_at: (reset != 0).then_some(reset) });
    let previous = model::Usage { five_hour: before(previous_reset[0]), seven_day: before(previous_reset[1]) };
    let mut marks: alert::Announced = [(*announced).min(3), (*announced.add(1)).min(3)];
    alert::rearm(&mut marks, &previous, &current, now);
    let lines = alert::check(&mut marks, &current, now).map(|a| a.lines.join("\n")).unwrap_or_default();
    if !copy_out(&lines, out, capacity, out_len) { return 1; }
    *announced = marks[0];
    *announced.add(1) = marks[1];
    0
}

/// The detail panel's pace line for one fresh reading (whether the quota should last
/// until it resets), from the shared `panel` rules, so the wording matches Windows.
/// The caller passes nothing after a failed fetch, as Windows hides the line then.
///
/// Writes the line (possibly empty) to `out` and whether it is a warning to `warn`.
/// Returns 0 on success, 1 on invalid arguments or a buffer that is too small.
/// # Safety
/// Non-null pointers must reference readable/writable memory of the stated sizes.
#[no_mangle]
pub unsafe extern "C" fn iusage_pace(
    usage: *const MacUsage, now: i64, utc_offset: i64,
    out: *mut u8, capacity: usize, out_len: *mut usize, warn: *mut bool,
) -> i32 {
    if usage.is_null() || out_len.is_null() || warn.is_null() { return 1; }
    *out_len = 0;
    *warn = false;
    let current = usage_of(&*usage);
    let ctx = panel::Context {
        current: model::Provider::Claude, show_remaining: false, thresholds: model::Thresholds::default(), now, utc_offset,
    };
    let (line, warning) = panel::pace(&current, &ctx);
    if !copy_out(&line, out, capacity, out_len) { return 1; }
    *warn = warning;
    0
}

fn usage_of(u: &MacUsage) -> model::Usage {
    let window = |pct: f64, reset: i64, has_window: bool, has_reset: bool| {
        has_window.then_some(model::Window { used_pct: pct, resets_at: has_reset.then_some(reset) })
    };
    model::Usage {
        five_hour: window(u.five_hour, u.five_reset, u.present & 1 != 0, u.present & 4 != 0),
        seven_day: window(u.seven_day, u.seven_reset, u.present & 2 != 0, u.present & 8 != 0),
    }
}

unsafe fn utf8<'a>(bytes: *const u8, len: usize) -> Option<&'a str> {
    if len == 0 { return Some(""); }
    if bytes.is_null() || len > 1_048_576 { return None; }
    std::str::from_utf8(std::slice::from_raw_parts(bytes, len)).ok()
}

unsafe fn copy_out(text: &str, out: *mut u8, capacity: usize, out_len: *mut usize) -> bool {
    if out.is_null() || out_len.is_null() || text.len() > capacity { return false; }
    std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len());
    *out_len = text.len();
    true
}

/// Decide whether the stored Claude credentials JSON needs renewing and whether that is
/// safe to try, without any I/O. Whether the store takes a write is the caller's check,
/// made after this returns 0 and before anything is sent.
///
/// Returns 0: POST the JSON written to `body` to the token endpoint; 1: not expired
/// (someone renewed it already); 2: expired and not renewable (no usable refresh token,
/// it has run out, or the server refused it before); 3: the layout cannot be patched
/// safely; 4: the credentials are unreadable; 5: invalid arguments or buffer too small.
/// # Safety
/// Non-null pointers must reference readable/writable memory of the stated lengths.
#[no_mangle]
pub unsafe extern "C" fn iusage_claude_refresh_plan(
    credentials: *const u8, len: usize, now: i64, body: *mut u8, capacity: usize, body_len: *mut usize,
) -> i32 {
    if body_len.is_null() { return 5; }
    *body_len = 0;
    let Some(stored) = utf8(credentials, len).filter(|t| !t.is_empty()) else { return 5; };
    match providers::plan_claude_refresh(stored, now, String::new(), &mut || true) {
        Ok(None) => 1,
        Ok(Some(plan)) => {
            if copy_out(plan.request.body.as_deref().unwrap_or(""), body, capacity, body_len) { 0 } else { 5 }
        }
        Err(model::FetchError::Expired) => 2,
        Err(model::FetchError::CannotSave) => 3,
        Err(_) => 4,
    }
}

/// Interpret the token endpoint's answer to a request made from `original`.
/// `current` is the store read again after the answer; it is only looked at once the
/// answer is a usable renewal, so it may be empty for any other status.
///
/// Returns 0: write the JSON in `out` to the store; 1: leave the store alone, another
/// process renewed meanwhile; 2: refresh token refused (it is not offered again);
/// 3: rate limited; 4: other HTTP status; 5: unusable response body; 6: the store cannot be
/// patched safely; 7: the stored credentials are unreadable; 8: invalid arguments.
/// # Safety
/// Non-null pointers must reference readable/writable memory of the stated lengths.
#[no_mangle]
pub unsafe extern "C" fn iusage_claude_refresh_finish(
    original: *const u8, original_len: usize, status: u16, retry_after: u32,
    response: *const u8, response_len: usize, current: *const u8, current_len: usize, now: i64,
    out: *mut u8, capacity: usize, out_len: *mut usize,
) -> i32 {
    if out_len.is_null() { return 8; }
    *out_len = 0;
    let Some(original) = utf8(original, original_len).filter(|t| !t.is_empty()) else { return 8; };
    let Ok(Some(plan)) = providers::plan_claude_refresh(original, now, String::new(), &mut || true) else { return 8; };
    // A body that is not UTF-8 is no usable renewal either; it is reported as such.
    let body = utf8(response, response_len).unwrap_or("\u{0}");
    let reply = providers::Response { status, body: body.to_string(), retry_after: (retry_after > 0).then_some(retry_after) };
    let mut read_current = || {
        utf8(current, current_len).filter(|t| !t.is_empty()).map(String::from).ok_or(model::FetchError::NoCredentials)
    };
    match providers::finish_claude_refresh(&plan, &reply, now, &mut read_current) {
        Ok(None) => 1,
        Ok(Some(updated)) => if copy_out(&updated, out, capacity, out_len) { 0 } else { 8 },
        Err(model::FetchError::Unauthorized) => 2,
        Err(model::FetchError::RateLimited(_)) => 3,
        Err(model::FetchError::Http(_) | model::FetchError::RefreshTemporary(_)) => 4,
        Err(model::FetchError::Parse) => 5,
        Err(model::FetchError::CannotSave) => 6,
        Err(model::FetchError::NoCredentials) => 7,
        Err(_) => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ffi_failure_clears_previous_reading() {
        let mut out = MacUsage { five_hour: 99., present: 1, ..Default::default() };
        unsafe { assert_eq!(iusage_parse(0, b"{}".as_ptr(), 2, &mut out), 2); }
        assert_eq!(out.present, 0);
        assert_eq!(out.five_hour, 0.);
        unsafe { assert_eq!(iusage_parse(3, b"{}".as_ptr(), 2, &mut out), 1); }
    }
    #[test]
    fn ffi_rejects_invalid_buffers_without_parsing() {
        let mut out = MacUsage::default();
        unsafe {
            assert_eq!(iusage_parse(0, std::ptr::null(), 0, &mut out), 1);
            assert_eq!(iusage_parse(0, b"{}".as_ptr(), 2, std::ptr::null_mut()), 1);
            assert_eq!(iusage_parse(0, [255u8].as_ptr(), 1, &mut out), 1);
            assert_eq!(iusage_parse(0, b"{}".as_ptr(), 1_048_577, &mut out), 1);
            assert!(!iusage_can_request(std::ptr::null(), 1000));
        }
    }
    #[test]
    fn ffi_distinguishes_absent_window_and_zero_usage() {
        let body = br#"{"five_hour":{"utilization":0},"seven_day":null}"#;
        let mut out = MacUsage::default();
        unsafe { assert_eq!(iusage_parse(0, body.as_ptr(), body.len(), &mut out), 0); }
        assert_eq!(out.present, 1);
    }
    #[test]
    fn ffi_reserves_gap_and_preserves_rejection() {
        let mut gate = MacGate::default();
        unsafe {
            assert_eq!(iusage_min_gap_secs(), schedule::MIN_GAP_SECS);
            assert!(iusage_start(&mut gate, 1000));
            assert!(!iusage_start(&mut gate, 1049));
            assert_eq!(gate.last_attempt, 1000);
            assert!(iusage_can_request(&gate, 1050));
        }
    }

    fn reading(five: Option<(f64, i64)>, seven: Option<(f64, i64)>) -> MacUsage {
        let mut u = MacUsage::default();
        if let Some((pct, reset)) = five { u.five_hour = pct; u.five_reset = reset; u.present |= 1 | 4; }
        if let Some((pct, reset)) = seven { u.seven_day = pct; u.seven_reset = reset; u.present |= 2 | 8; }
        u
    }
    fn alert_check(u: &MacUsage, announced: &mut [u8; 2], previous: [i64; 2], now: i64) -> (i32, String) {
        let mut out = vec![0u8; 512];
        let mut len = 0usize;
        let code = unsafe { iusage_alert_check(u, announced.as_mut_ptr(), previous.as_ptr(), now, out.as_mut_ptr(), out.len(), &mut len) };
        (code, String::from_utf8(out[..len].to_vec()).unwrap())
    }

    #[test]
    fn ffi_alerts_follow_the_shared_remaining_percentage_rules() {
        let now = 1_000_000;
        let (five_reset, seven_reset) = (now + 7200 + 30, now + 3 * 86_400 + 30);
        let mut announced = [0u8; 2];
        // Quiet above 20% left.
        assert_eq!(alert_check(&reading(Some((79., five_reset)), Some((10., seven_reset))), &mut announced, [0, 0], now), (0, String::new()));
        // 20% left: one line with the same wording as Windows, then silence.
        let high = reading(Some((80., five_reset)), Some((10., seven_reset)));
        assert_eq!(alert_check(&high, &mut announced, [five_reset, seven_reset], now), (0, "5 小时额度剩余 20%，2h 后重置".to_string()));
        assert_eq!(announced, [1, 0]);
        assert_eq!(alert_check(&high, &mut announced, [five_reset, seven_reset], now).1, "");
        // 10% and 5% left each fire once.
        assert!(alert_check(&reading(Some((90., five_reset)), None), &mut announced, [five_reset, 0], now).1.contains("剩余 10%"));
        assert!(alert_check(&reading(Some((95., five_reset)), None), &mut announced, [five_reset, 0], now).1.contains("剩余 5%"));
        assert_eq!(announced[0], 3);
        // A reset time that only drifts later does not re-arm ...
        assert_eq!(alert_check(&reading(Some((95., five_reset + 3)), None), &mut announced, [five_reset, 0], now + 90).1, "");
        // ... but a window whose old reset has passed does.
        let later = five_reset + 60;
        let fresh = reading(Some((82., later + 18_000)), Some((10., seven_reset)));
        assert!(alert_check(&fresh, &mut announced, [five_reset, seven_reset], later).1.contains("剩余 18%"));
    }

    #[test]
    fn ffi_alert_rejects_bad_arguments_and_keeps_state() {
        let mut announced = [2u8, 1];
        let u = reading(Some((99., 5000)), None);
        let mut tiny = [0u8; 2];
        let mut len = 0usize;
        unsafe {
            let previous = [5000i64, 0];
            assert_eq!(iusage_alert_check(&u, announced.as_mut_ptr(), previous.as_ptr(), 1000, tiny.as_mut_ptr(), tiny.len(), &mut len), 1);
            assert_eq!(announced, [2, 1]);
            assert_eq!(iusage_alert_check(std::ptr::null(), announced.as_mut_ptr(), previous.as_ptr(), 1000, tiny.as_mut_ptr(), tiny.len(), &mut len), 1);
        }
    }

    fn stored(refresh: &str, expires_ms: i64) -> String {
        format!(r#"{{"other":{{"accessToken":"keep"}},"claudeAiOauth":{{"accessToken":"old","refreshToken":"{refresh}","expiresAt":{expires_ms},"scopes":["a","b"]}}}}"#)
    }
    const NOW: i64 = 1_790_900_000;
    fn plan(text: &str) -> (i32, String) {
        let mut body = vec![0u8; 4096];
        let mut len = 0usize;
        let code = unsafe { iusage_claude_refresh_plan(text.as_ptr(), text.len(), NOW, body.as_mut_ptr(), body.len(), &mut len) };
        (code, String::from_utf8(body[..len].to_vec()).unwrap())
    }
    fn finish(original: &str, status: u16, retry_after: u32, response: &str, current: &str) -> (i32, String) {
        let mut out = vec![0u8; original.len() + 16384];
        let mut len = 0usize;
        let code = unsafe {
            iusage_claude_refresh_finish(original.as_ptr(), original.len(), status, retry_after, response.as_ptr(), response.len(),
                                         current.as_ptr(), current.len(), NOW, out.as_mut_ptr(), out.len(), &mut len)
        };
        (code, String::from_utf8(out[..len].to_vec()).unwrap())
    }
    const RENEWED: &str = r#"{"access_token":"new-access","refresh_token":"ffi-new","expires_in":28800}"#;

    #[test]
    fn ffi_plans_a_renewal_only_for_an_expired_token() {
        let (code, body) = plan(&stored("ffi-plan", (NOW - 60) * 1000));
        assert_eq!(code, 0);
        assert!(body.contains(r#""refresh_token":"ffi-plan""#) && body.contains(r#""scope":"a b""#));
        assert_eq!(plan(&stored("ffi-plan", (NOW + 600) * 1000)), (1, String::new()));
        assert_eq!(plan(&stored("", (NOW - 60) * 1000)).0, 2);
        assert_eq!(plan("[]").0, 4);
        assert_eq!(plan("").0, 5);
        let duplicated = r#"{"claudeAiOauth":{"accessToken":"a","accessToken":"b","refreshToken":"ffi-dup","expiresAt":1000}}"#;
        assert_eq!(plan(duplicated).0, 3);
        let mut tiny = [0u8; 4];
        let mut len = 0usize;
        let text = stored("ffi-plan-tiny", (NOW - 60) * 1000);
        unsafe { assert_eq!(iusage_claude_refresh_plan(text.as_ptr(), text.len(), NOW, tiny.as_mut_ptr(), tiny.len(), &mut len), 5); }
    }

    #[test]
    fn ffi_finishes_a_renewal_without_touching_anything_else() {
        let before = stored("ffi-finish", (NOW - 60) * 1000);
        let (code, after) = finish(&before, 200, 0, RENEWED, &before);
        assert_eq!(code, 0);
        let expected = before.replace("old", "new-access").replace("ffi-finish", "ffi-new")
            .replace(&((NOW - 60) * 1000).to_string(), &((NOW + 28_800) * 1000).to_string());
        assert_eq!(after, expected);
        // Another process renewed first: leave the store alone.
        assert_eq!(finish(&before, 200, 0, RENEWED, &stored("theirs", (NOW + 900) * 1000)).0, 1);
        // Nothing usable to compare with.
        assert_eq!(finish(&before, 200, 0, RENEWED, "").0, 7);
        assert_eq!(finish(&before, 200, 0, "{}", &before).0, 5);
        assert_eq!(finish(&before, 200, 0, "\u{0}", &before).0, 5);
        assert_eq!(finish(&before, 429, 600, "", "").0, 3);
        assert_eq!(finish(&before, 503, 0, "", "").0, 4);
        assert_eq!(finish("not json", 200, 0, RENEWED, &before).0, 8);
    }

    #[test]
    fn ffi_does_not_offer_a_refused_token_again() {
        let before = stored("ffi-refused", (NOW - 60) * 1000);
        assert_eq!(plan(&before).0, 0);
        assert_eq!(finish(&before, 400, 0, r#"{"error":"invalid_grant"}"#, "").0, 2);
        assert_eq!(plan(&before).0, 2, "a local verdict now; nothing would be sent");
        assert_eq!(plan(&stored("ffi-refused-after-login", (NOW - 60) * 1000)).0, 0);
    }

    fn pace_line(u: &MacUsage, now: i64) -> (i32, String, bool) {
        let mut out = vec![0u8; 256];
        let (mut len, mut warn) = (0usize, true);
        let code = unsafe { iusage_pace(u, now, 8 * 3600, out.as_mut_ptr(), out.len(), &mut len, &mut warn) };
        (code, String::from_utf8(out[..len].to_vec()).unwrap(), warn)
    }

    #[test]
    fn ffi_pace_matches_the_windows_panel_line() {
        const DAY: i64 = 86_400;
        let now = 1_790_860_000; // Thursday 1 October 2026, 21:06 in UTC+8
        let steady = reading(None, Some((20., now + 5 * DAY)));
        assert_eq!(pace_line(&steady, now), (0, "进度正常，星期二 21:06 重置前额度充裕".into(), false));
        let fast = reading(None, Some((40., now + 5 * DAY)));
        assert_eq!(pace_line(&fast, now), (0, "额度用得较快，预计 星期日 21:06 用完".into(), true));
        let session = reading(Some((90., now + 2 * 3600)), Some((10., now + 5 * DAY)));
        assert_eq!(pace_line(&session, now), (0, "5 小时额度用得较快，预计 今天 21:26 用完".into(), true));
        // No reset time, or too early in the window: no line.
        let mut unknown = reading(None, Some((50., 0)));
        unknown.present &= !8;
        assert_eq!(pace_line(&unknown, now), (0, String::new(), false));
        assert_eq!(pace_line(&MacUsage::default(), now), (0, String::new(), false));
        // A buffer too small leaves nothing half-written.
        let (mut len, mut warn) = (7usize, true);
        let mut tiny = [0u8; 4];
        assert_eq!(unsafe { iusage_pace(&fast, now, 8 * 3600, tiny.as_mut_ptr(), tiny.len(), &mut len, &mut warn) }, 1);
        assert_eq!((len, warn), (0, false));
        assert_eq!(unsafe { iusage_pace(std::ptr::null(), now, 0, tiny.as_mut_ptr(), 4, &mut len, &mut warn) }, 1);
    }
}
