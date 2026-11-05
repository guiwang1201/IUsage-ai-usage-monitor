//! When the usage service may be asked. Its rate limit is unpublished and, once hit,
//! takes minutes to clear, so every path to a request goes through these rules.

use crate::model::FetchError;

/// Shortest time between two requests that reached the same service, whatever asked
/// for the second one: the schedule, a provider switch, the panel or "refresh now".
pub const MIN_GAP_SECS: i64 = 50;
/// How often the reading is refreshed while nobody is at the machine.
pub const IDLE_POLL_SECS: i64 = 900;
/// Bounds ordinary poll scheduling after a clock change, never a server cooldown.
pub const MAX_WAIT_SECS: i64 = 2 * 3600;

/// Persisted per-provider request gate, shared by the UI and diagnostic mode.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct State {
    pub last_attempt: i64,
    pub next_poll: i64,
    pub blocked_until: i64,
    pub backoff: i64,
}

impl State {
    pub fn can_request(&self, now: i64) -> bool {
        !told_to_wait(self.blocked_until, now) && gap_elapsed(self.last_attempt, now)
    }

    pub fn due(&self, last_success: Option<i64>, away: bool, now: i64) -> bool {
        self.can_request(now) && poll_due(self.next_poll, self.last_attempt, last_success, away, now)
    }

    /// Reserve the gap before starting I/O, including if the process exits mid-request.
    pub fn started(&mut self, now: i64) {
        self.last_attempt = now;
        self.blocked_until = now + MIN_GAP_SECS;
    }

    pub fn succeeded(&mut self, now: i64, poll_secs: u32) {
        self.last_attempt = now;
        self.next_poll = now + i64::from(poll_secs);
        self.blocked_until = 0;
        self.backoff = 0;
    }

    pub fn failed(&mut self, error: &FetchError, now: i64) {
        // Only a credential failure proves that no HTTP request was sent. A timeout
        // can happen after the server received it, so it must also consume the gap.
        if matches!(error, FetchError::NoCredentials | FetchError::Expired) {
            self.last_attempt = 0;
            self.blocked_until = now + 15;
            self.next_poll = self.blocked_until;
            return;
        }
        let minimum = if matches!(error, FetchError::RateLimited(_) | FetchError::Unauthorized | FetchError::RefreshTemporary(_) | FetchError::WebUnauthorized | FetchError::WebForbidden | FetchError::WebCredentials | FetchError::WebOrganization) { 300 } else { 30 };
        self.backoff = self.backoff.saturating_mul(2).clamp(minimum, 960);
        let retry_after = match error {
            FetchError::RateLimited(Some(seconds)) => i64::from(*seconds),
            _ => 0,
        };
        self.last_attempt = now;
        self.blocked_until = now + self.backoff.max(retry_after).max(MIN_GAP_SECS);
        self.next_poll = self.blocked_until;
    }
}

/// True once enough time has passed since the service last answered.
pub fn gap_elapsed(last_response: i64, now: i64) -> bool {
    // An answer "in the future" means the clock was set back; do not wait for it.
    last_response > now || now.saturating_sub(last_response) >= MIN_GAP_SECS
}

/// True while a wait the service imposed (rate limit, rejected token) is still running.
/// Nothing may ask during it: not the schedule, not a wake from sleep, not the user.
pub fn told_to_wait(blocked_until: i64, now: i64) -> bool {
    // A server-imposed wait is authoritative, including across restarts and clock changes.
    blocked_until > now
}

/// Whether the regular schedule should send a request now.
///
/// `away` is true when nobody is at the machine; a reading younger than
/// `IDLE_POLL_SECS` is then good enough.
pub fn poll_due(next_poll: i64, last_response: i64, last_success: Option<i64>, away: bool, now: i64) -> bool {
    let scheduled = now >= next_poll || next_poll.saturating_sub(now) > MAX_WAIT_SECS;
    if !scheduled || !gap_elapsed(last_response, now) {
        return false;
    }
    !(away && last_success.is_some_and(|at| (0..IDLE_POLL_SECS).contains(&now.saturating_sub(at))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    #[test]
    fn fifty_second_poll_preserves_idle_and_server_waits() {
        let mut state = State::default();
        state.succeeded(NOW, 50);
        assert!(!state.due(Some(NOW), false, NOW + 49));
        assert!(state.due(Some(NOW), false, NOW + 50));
        assert!(!state.due(Some(NOW), true, NOW + 50));
        assert!(state.due(Some(NOW), true, NOW + IDLE_POLL_SECS));
        state.failed(&FetchError::RateLimited(Some(1800)), NOW + 50);
        assert!(!state.due(Some(NOW), false, NOW + 1849));
        assert!(state.due(Some(NOW), false, NOW + 1850));
    }

    #[test]
    fn repeated_limits_really_double_and_survive_a_copy() {
        let mut state = State::default();
        for delay in [300, 600, 960, 960] {
            state.failed(&FetchError::RateLimited(None), NOW);
            let restored = state;
            assert_eq!(restored.blocked_until, NOW + delay);
            assert!(!restored.can_request(NOW + delay - 1));
            assert!(restored.can_request(NOW + delay));
        }
        state.failed(&FetchError::RateLimited(Some(1800)), NOW);
        assert!(!state.can_request(NOW + 1799));
        state.succeeded(NOW, 180);
        assert_eq!(state.backoff, 0);
    }

    #[test]
    fn timeouts_and_in_flight_requests_cannot_bypass_the_gap() {
        let mut state = State::default();
        state.started(NOW);
        assert!(!state.can_request(NOW + 49));
        state.failed(&FetchError::Network("receive timeout".into()), NOW + 10);
        assert!(!state.can_request(NOW + 59));
        assert!(state.can_request(NOW + 60));
        // Waking or forcing the periodic schedule cannot remove the error wait.
        state.next_poll = 0;
        assert!(!state.due(None, false, NOW + 20));
    }

    #[test]
    fn only_local_credentials_are_rechecked_quickly() {
        let mut state = State::default();
        state.started(NOW);
        state.failed(&FetchError::Expired, NOW);
        assert!(!state.can_request(NOW + 14));
        assert!(state.can_request(NOW + 15));
    }

    #[test]
    fn server_waits_are_not_capped_or_bypassed_by_clock_changes() {
        let mut state = State::default();
        state.failed(&FetchError::RateLimited(Some(86_400)), NOW);
        assert_eq!(state.blocked_until, NOW + 86_400);
        state.next_poll = 0;
        assert!(!state.due(None, false, NOW + 7201));
        assert!(!state.can_request(NOW - 3600));
        assert!(state.can_request(NOW + 86_400));
        state.failed(&FetchError::RefreshTemporary(403), NOW);
        assert!(!state.can_request(NOW + 299));
    }

    #[test]
    fn polls_when_the_scheduled_time_has_come() {
        assert!(!poll_due(NOW + 60, NOW - 120, Some(NOW - 120), false, NOW));
        assert!(poll_due(NOW, NOW - 180, Some(NOW - 180), false, NOW));
        assert!(poll_due(0, 0, None, false, NOW), "first run, nothing known yet");
    }

    #[test]
    fn nothing_gets_past_the_minimum_gap() {
        // Scheduled time reached (a provider switch, a forced refresh), but the service
        // answered 30 seconds ago.
        assert!(!poll_due(0, NOW - 30, Some(NOW - 30), false, NOW));
        assert!(!gap_elapsed(NOW - (MIN_GAP_SECS - 1), NOW));
        assert!(gap_elapsed(NOW - MIN_GAP_SECS, NOW));
    }

    #[test]
    fn slows_down_while_nobody_is_there() {
        let last = NOW - 300;
        assert!(poll_due(NOW - 120, last, Some(last), false, NOW), "someone is there: normal cadence");
        assert!(!poll_due(NOW - 120, last, Some(last), true, NOW), "away: five minutes old is fine");
        let old = NOW - IDLE_POLL_SECS;
        assert!(poll_due(NOW - 720, old, Some(old), true, NOW), "away: refreshed once it is 15 minutes old");
        assert!(poll_due(0, 0, None, true, NOW), "away with no reading at all still tries");
    }

    #[test]
    fn an_imposed_wait_runs_its_course() {
        assert!(told_to_wait(NOW + 300, NOW));
        assert!(told_to_wait(NOW + 1, NOW));
        assert!(!told_to_wait(NOW, NOW));
        assert!(!told_to_wait(0, NOW), "nothing imposed");
        assert!(told_to_wait(NOW + MAX_WAIT_SECS + 1, NOW), "long server waits are preserved");
    }

    #[test]
    fn a_clock_set_back_does_not_stall_polling() {
        assert!(poll_due(NOW + MAX_WAIT_SECS + 1, NOW - 500, Some(NOW - 500), false, NOW));
        assert!(!poll_due(NOW + MAX_WAIT_SECS, NOW - 500, Some(NOW - 500), false, NOW));
        assert!(gap_elapsed(NOW + 5000, NOW));
        assert!(poll_due(0, NOW + 5000, Some(NOW + 5000), true, NOW));
    }
}
