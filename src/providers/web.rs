//! Optional, explicitly configured Claude web session. Never falls through to OAuth
//! after a web request: both channels share the caller's persisted provider gate.
use super::*;

pub struct Session {
    pub key: String,
    pub organization: Option<String>,
}

impl Session {
    pub fn from_input(input: &str, organization: &str) -> Result<Self, FetchError> {
        let input = input.trim();
        let key = input.strip_prefix("sessionKey=").unwrap_or(input).trim_end_matches(';').trim().to_string();
        let organization = (!organization.trim().is_empty()).then(|| organization.trim().to_string());
        let session = Self { key, organization };
        session.validate()?;
        Ok(session)
    }
    pub fn from_env() -> Result<Option<Self>, FetchError> {
        let Some(key) = std::env::var_os("IUSAGE_CLAUDE_SESSION_KEY") else { return Ok(None) };
        let key = key.into_string().map_err(|_| FetchError::WebCredentials)?;
        let organization = std::env::var_os("IUSAGE_CLAUDE_ORG_ID")
            .map(|s| s.into_string().map_err(|_| FetchError::WebOrganization)).transpose()?.filter(|s| !s.is_empty());
        let session = Self { key, organization };
        session.validate()?;
        Ok(Some(session))
    }

    fn validate(&self) -> Result<(), FetchError> {
        // Accept the cookie value only, never an arbitrary Cookie header.
        if self.key.is_empty() || self.key.len() > 8192
            || !self.key.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b';' | b',' | b'"' | b'\\')) {
            return Err(FetchError::WebCredentials);
        }
        if self.organization.as_deref().is_some_and(|s| !valid_org(s)) {
            return Err(FetchError::WebOrganization);
        }
        Ok(())
    }

    fn request(&self, path: String) -> Request {
        Request {
            host: "claude.ai", path, user_agent: USER_AGENT.into(), body: None,
            headers: vec![
                ("Cookie", format!("sessionKey={}", self.key)),
                ("Accept", "application/json".into()),
                ("Origin", "https://claude.ai".into()),
                ("Referer", "https://claude.ai/settings/usage".into()),
            ],
        }
    }
}

fn valid_org(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn send(request: &Request, http: Http) -> Result<String, FetchError> {
    let response = http(request).map_err(transport)?;
    match response.status {
        200 => Ok(response.body),
        401 => Err(FetchError::WebUnauthorized),
        403 => Err(FetchError::WebForbidden),
        429 => Err(FetchError::RateLimited(response.retry_after)),
        status => Err(FetchError::Http(status)),
    }
}

pub fn fetch(session: &Session, http: Http, before_network: &mut dyn FnMut() -> Result<(), FetchError>) -> Result<Usage, FetchError> {
    session.validate()?;
    let organization = match &session.organization {
        Some(id) => id.clone(),
        None => {
            before_network()?;
            let body = send(&session.request("/api/organizations".into()), http)?;
            let root = json::parse(&body).ok_or(FetchError::Parse)?;
            let JsonValue::Array(orgs) = root else { return Err(FetchError::Parse) };
            // Do not silently select a different organization's quota.
            if orgs.len() != 1 { return Err(FetchError::WebOrganization) }
            get(&orgs[0], "uuid").and_then(text).filter(|s| valid_org(s))
                .ok_or(FetchError::WebOrganization)?.to_string()
        }
    };
    before_network()?;
    let body = send(&session.request(format!("/api/organizations/{organization}/usage")), http)?;
    read_answer(Provider::Claude, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    thread_local! { static CALLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) }; }
    fn fake(request: &Request) -> Result<Response, String> {
        CALLS.with(|calls| calls.borrow_mut().push(request.path.clone()));
        assert_eq!(request.host, "claude.ai");
        assert!(request.headers.iter().any(|(k, v)| *k == "Cookie" && v == "sessionKey=test-key"));
        let body = if request.path == "/api/organizations" { r#"[{"uuid":"org-1"}]"# }
            else { r#"{"five_hour":{"utilization":42},"seven_day":null}"# };
        Ok(Response { status: 200, body: body.into(), retry_after: None })
    }
    #[test]
    fn discovers_one_organization_and_fetches_only_quota() {
        CALLS.with(|c| c.borrow_mut().clear());
        let mut reservations = 0;
        let usage = fetch(&Session { key: "test-key".into(), organization: None }, fake, &mut || { reservations += 1; Ok(()) }).unwrap();
        assert_eq!(usage.five_hour.unwrap().used_pct, 42.0);
        assert_eq!(reservations, 2);
        CALLS.with(|c| assert_eq!(*c.borrow(), ["/api/organizations", "/api/organizations/org-1/usage"]));
    }
    #[test]
    fn explicit_organization_needs_only_one_request() {
        CALLS.with(|c| c.borrow_mut().clear());
        fetch(&Session { key: "test-key".into(), organization: Some("org-1".into()) }, fake, &mut || Ok(())).unwrap();
        CALLS.with(|c| assert_eq!(c.borrow().len(), 1));
    }
    #[test]
    fn rate_limit_and_persistence_failure_stop_before_next_request() {
        let session = Session { key: "test-key".into(), organization: None };
        let limited: Http = |_| Ok(Response { status: 429, body: String::new(), retry_after: Some(7200) });
        assert_eq!(fetch(&session, limited, &mut || Ok(())), Err(FetchError::RateLimited(Some(7200))));
        let never: Http = |_| panic!("must not send");
        assert_eq!(fetch(&session, never, &mut || Err(FetchError::CannotSave)), Err(FetchError::CannotSave));
        for key in ["", "x\r\nInjected: y", "x; other=y"] {
            assert_eq!(fetch(&Session { key: key.into(), organization: None }, never, &mut || Ok(())), Err(FetchError::WebCredentials));
        }
    }
    #[test]
    fn an_organization_without_quota_windows_is_unsubscribed() {
        let empty: Http = |_| Ok(Response { status: 200, body: r#"{"five_hour":null,"seven_day":null}"#.into(), retry_after: None });
        let session = Session { key: "test-key".into(), organization: Some("org-1".into()) };
        assert_eq!(fetch(&session, empty, &mut || Ok(())), Err(FetchError::Unsubscribed));
    }
    #[test]
    fn ambiguous_organizations_are_not_selected() {
        let many: Http = |_| Ok(Response { status: 200, body: r#"[{"uuid":"one"},{"uuid":"two"}]"#.into(), retry_after: None });
        assert_eq!(fetch(&Session { key: "test-key".into(), organization: None }, many, &mut || Ok(())), Err(FetchError::WebOrganization));
    }

    #[test]
    fn selected_web_failure_never_falls_through_to_oauth() {
        let env = Env { home: PathBuf::new(), claude_dir: PathBuf::new(), claude_code_dirs: vec![] };
        let session = Session { key: "test-key".into(), organization: None };
        let mut reservations = 0;
        let limited: Http = |request| {
            assert_eq!(request.host, "claude.ai");
            assert_eq!(request.path, "/api/organizations");
            Ok(Response { status: 429, body: String::new(), retry_after: Some(86400) })
        };
        assert_eq!(fetch_selected(Provider::Claude, &env, 0, limited, Some(&session), &mut || { reservations += 1; Ok(()) }),
            Err(FetchError::RateLimited(Some(86400))));
        assert_eq!(reservations, 1);
    }
}
