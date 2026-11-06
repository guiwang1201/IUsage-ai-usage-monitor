//! HTTPS GET and POST through WinHTTP: no extra code in the binary, and it follows the
//! system proxy.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::*;

use super::wide;
use crate::providers::{Request, Response};

const MAX_BODY: usize = 1 << 20;

fn retry_after(value: &str, now: i64) -> Option<u32> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value.parse::<u64>().ok().map(|n| n.min(u32::MAX as u64) as u32);
    }
    // IMF-fixdate, the HTTP date emitted by current servers. The Windows parser
    // accepts even malformed strings using current-time defaults, so validate here.
    if value.len() != 29 || !value.is_ascii() || &value[3..5] != ", " || &value[25..] != " GMT" {
        return None;
    }
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = months.iter().position(|m| *m == &value[8..11])? + 1;
    let iso = format!("{}-{month:02}-{}T{}Z", &value[12..16], &value[5..7], &value[17..25]);
    let unix = crate::timefmt::parse_iso8601(&iso)?;
    Some(unix.saturating_sub(now).clamp(0, i64::from(u32::MAX)) as u32)
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn failed(step: &str) -> String {
    format!("{step} failed ({})", unsafe { GetLastError() })
}

/// A failed send. A name that does not resolve or a connection that cannot be made
/// (to the host or the proxy) proves that no server received the request; a timeout or
/// a dropped connection does not.
fn send_failed(code: u32) -> String {
    let not_sent = matches!(code, ERROR_WINHTTP_NAME_NOT_RESOLVED | ERROR_WINHTTP_CANNOT_CONNECT);
    format!("{}WinHttpSendRequest failed ({code})", if not_sent { crate::providers::NOT_SENT } else { "" })
}

/// Sends a POST when the request has a body, a GET otherwise.
pub fn send(request: &Request) -> Result<Response, String> {
    let started = Instant::now();
    let agent = wide(&request.user_agent);
    let host = wide(request.host);
    let path = wide(&request.path);
    let verb = wide(if request.body.is_some() { "POST" } else { "GET" });
    let body = request.body.as_deref().unwrap_or_default().as_bytes();
    let body_len = u32::try_from(body.len()).map_err(|_| "Request body too large".to_string())?;
    let headers: String = request.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let headers = wide(&headers);

    unsafe {
        let session = Handle(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
        if session.0.is_null() {
            return Err(failed("WinHttpOpen"));
        }
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 15_000);

        let connection = Handle(WinHttpConnect(session.0, host.as_ptr(), 443, 0));
        if connection.0.is_null() {
            return Err(failed("WinHttpConnect"));
        }
        let req = Handle(WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            path.as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err(failed("WinHttpOpenRequest"));
        }
        // Never follow a redirect: the request carries a bearer token, and following one
        // would hand that token to whatever host the redirect names. Cookies are not
        // needed either. If this cannot be set, nothing is sent.
        let disabled: u32 = WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_COOKIES;
        if WinHttpSetOption(
            req.0,
            WINHTTP_OPTION_DISABLE_FEATURE,
            &disabled as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        ) == 0
        {
            return Err(failed("WinHttpSetOption"));
        }
        if WinHttpAddRequestHeaders(req.0, headers.as_ptr(), u32::MAX, WINHTTP_ADDREQ_FLAG_ADD) == 0 {
            return Err(failed("WinHttpAddRequestHeaders"));
        }
        let data = if body.is_empty() { null() } else { body.as_ptr() as *const c_void };
        if WinHttpSendRequest(req.0, null(), 0, data, body_len, body_len, 0) == 0 {
            return Err(send_failed(GetLastError()));
        }
        if WinHttpReceiveResponse(req.0, null_mut()) == 0 {
            return Err(failed("WinHttpReceiveResponse"));
        }

        let mut status: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut c_void,
            &mut size,
            null_mut(),
        ) == 0
        {
            return Err(failed("WinHttpQueryHeaders"));
        }
        // Retry-After can be either seconds or an HTTP date. Both impose a wait.
        let mut value = [0u16; 128];
        let mut size = std::mem::size_of_val(&value) as u32;
        let retry_after = if WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_RETRY_AFTER,
            null(),
            value.as_mut_ptr() as *mut c_void,
            &mut size,
            null_mut(),
        ) != 0 {
            let len = value.iter().position(|c| *c == 0).unwrap_or(value.len());
            retry_after(&String::from_utf16_lossy(&value[..len]), super::unix_now())
        } else { None };

        // Read only bounded refresh 400/401 bodies to identify invalid_grant.
        // Act on 429 immediately, without waiting for an error body.
        let refresh_error = request.host == "platform.claude.com"
            && request.path == "/v1/oauth/token" && matches!(status, 400 | 401);
        if status != 200 && !refresh_error {
            return Ok(Response { status: status as u16, body: String::new(), retry_after });
        }

        let mut body = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if started.elapsed() >= Duration::from_secs(60) {
                return Err("Response exceeded total time limit".into());
            }
            let mut read: u32 = 0;
            if WinHttpReadData(req.0, chunk.as_mut_ptr() as *mut c_void, chunk.len() as u32, &mut read) == 0 {
                return Err(failed("WinHttpReadData"));
            }
            if read == 0 {
                break;
            }
            if body.len() + read as usize > if refresh_error { 16 * 1024 } else { MAX_BODY } {
                return Err("Response exceeded size limit".into());
            }
            body.extend_from_slice(&chunk[..read as usize]);
        }
        let body = String::from_utf8(body).map_err(|_| "Response is not UTF-8".to_string())?;
        Ok(Response { status: status as u16, body, retry_after })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_accepts_seconds_and_http_dates_without_network() {
        assert_eq!(retry_after("600", 0), Some(600));
        assert_eq!(retry_after("Thu, 01 Oct 2026 17:30:00 GMT", 1_790_875_200), Some(600));
        assert_eq!(retry_after("Thu, 01 Oct 2026 17:30:00 GMT", 1_790_876_000), Some(0));
        assert_eq!(retry_after("invalid", 0), None);
        assert_eq!(retry_after("", 0), None);
    }

    #[test]
    fn only_failures_before_any_server_saw_the_request_count_as_offline() {
        use crate::model::FetchError;
        use crate::providers::transport;
        assert!(matches!(transport(send_failed(ERROR_WINHTTP_NAME_NOT_RESOLVED)), FetchError::Offline(_)));
        assert!(matches!(transport(send_failed(ERROR_WINHTTP_CANNOT_CONNECT)), FetchError::Offline(_)));
        for ambiguous in [ERROR_WINHTTP_TIMEOUT, ERROR_WINHTTP_CONNECTION_ERROR, ERROR_WINHTTP_SECURE_FAILURE] {
            assert!(matches!(transport(send_failed(ambiguous)), FetchError::Network(_)));
        }
    }
}
