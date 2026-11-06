//! Platform-independent data model.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn index(self) -> usize {
        match self {
            Provider::Claude => 0,
            Provider::Codex => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }

    pub fn other(self) -> Provider {
        match self {
            Provider::Claude => Provider::Codex,
            Provider::Codex => Provider::Claude,
        }
    }

    /// Stable identifier used in the settings file.
    pub fn key(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    pub fn from_key(key: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.key() == key)
    }
}

/// One rate-limit window.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Window {
    /// Percent of the window already used, 0..=100.
    pub used_pct: f64,
    /// Unix seconds at which the window resets, if the service reported it.
    pub resets_at: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

/// Used-percent levels at which a bar turns yellow, then red, and a notification is sent.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Thresholds {
    pub warn: i64,
    pub crit: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { warn: 75, crit: 90 }
    }
}

impl Thresholds {
    /// 0 below both thresholds, 1 from `warn`, 2 from `crit`.
    pub fn level(self, used_pct: i64) -> u8 {
        if used_pct >= self.crit {
            2
        } else if used_pct >= self.warn {
            1
        } else {
            0
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum FetchError {
    /// Credential file missing or unreadable: the desktop app is not signed in.
    NoCredentials,
    /// The stored token is past its expiry and cannot be renewed: there is no refresh
    /// token, or it has run out too. No request was sent.
    Expired,
    /// The service answered 401 or 403 to a token that looked valid, or turned down the
    /// refresh token.
    Unauthorized,
    /// A renewed token could not be written back to the credential file, so it was not
    /// requested at all.
    CannotSave,
    /// HTTP 429, with the server's Retry-After in seconds when it sent one.
    RateLimited(Option<u32>),
    /// Refresh failed without proof that the refresh token is permanently invalid.
    RefreshTemporary(u16),
    WebCredentials,
    WebUnauthorized,
    WebForbidden,
    WebOrganization,
    /// Restored cooldown proves the previous request failed; detailed errors are not persisted.
    PreviousFailure,
    /// The service answered, but the account has no paid quota: Codex reports a free
    /// plan, Claude reports no window at all. There is nothing worth polling.
    Unsubscribed,
    Http(u16),
    Network(String),
    /// The request never left this machine: the name did not resolve or no connection
    /// could be made. No server saw it, so it costs nothing against a rate limit.
    Offline(String),
    Parse,
}
