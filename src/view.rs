//! Turns usage data into what the widget shows. Platform-independent.

use crate::model::{FetchError, Provider, Thresholds, Usage, Window};
use crate::timefmt::countdown;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Severity {
    Ok,
    Warn,
    Crit,
}

impl Severity {
    /// From a `Thresholds::level`.
    pub fn of(level: u8) -> Severity {
        match level {
            0 => Severity::Ok,
            1 => Severity::Warn,
            _ => Severity::Crit,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Row {
    pub label: &'static str,
    /// Filled share of the bar, 0..=1. `None` draws an empty, dimmed bar.
    pub fill: Option<f32>,
    pub severity: Severity,
    pub text: String,
}

#[derive(Clone, PartialEq, Debug)]
pub struct View {
    pub provider: Provider,
    pub rows: [Row; 2],
    /// Numbers are from an earlier successful fetch; the latest one failed.
    pub stale: bool,
    /// Multi-line tooltip text.
    pub tip: String,
}

impl View {
    /// Tooltip updates do not change the taskbar bitmap.
    pub fn same_pixels(&self, other: &Self) -> bool {
        self.provider == other.provider && self.rows == other.rows && self.stale == other.stale
    }
}

pub fn build(
    provider: Provider,
    usage: Option<&Usage>,
    error: Option<&FetchError>,
    show_remaining: bool,
    thresholds: Thresholds,
    now: i64,
) -> View {
    let mut tip = format!("IUsage · {}", provider.name());
    let rows = match usage {
        Some(usage) => {
            let rows = [
                row("5h", usage.five_hour, error, show_remaining, thresholds, now),
                row("7d", usage.seven_day, error, show_remaining, thresholds, now),
            ];
            let what = if show_remaining { "剩余" } else { "已用" };
            for r in &rows {
                if r.fill.is_none() {
                    continue;
                }
                let Some((pct, reset)) = r.text.split_once(' ') else { continue };
                tip.push_str(&format!("\n{} {what} {pct}", r.label));
                if reset != "--" {
                    tip.push_str(&format!(" · {reset} 后重置"));
                }
            }
            rows
        }
        None => {
            let status = error.map_or("…".to_string(), short_label);
            [empty_row("5h", &status), empty_row("7d", "--")]
        }
    };
    if let Some(error) = error {
        tip.push('\n');
        tip.push_str(&describe(provider, error));
    }
    View { provider, rows, stale: usage.is_some() && error.is_some(), tip }
}

/// Whole percent used right now. Once the reset time has passed the window is empty
/// again, whatever the last fetch said.
pub fn used_now(window: &Window, now: i64) -> i64 {
    match window.resets_at {
        Some(reset) if reset <= now => 0,
        _ => (window.used_pct.round() as i64).clamp(0, 100),
    }
}

/// True when the window has reset since the reading was taken. After a failed fetch
/// nothing is known about the new window: it may well have been used since.
pub fn reset_since(window: &Window, now: i64) -> bool {
    window.resets_at.is_some_and(|reset| reset <= now)
}

fn row(
    label: &'static str,
    window: Option<Window>,
    error: Option<&FetchError>,
    show_remaining: bool,
    thresholds: Thresholds,
    now: i64,
) -> Row {
    let Some(window) = window else { return empty_row(label, "--") };
    if let Some(error) = error.filter(|_| reset_since(&window, now)) {
        return empty_row(label, &short_label(error));
    }
    let used = used_now(&window, now);
    let shown = if show_remaining { 100 - used } else { used };
    Row {
        label,
        fill: Some(shown as f32 / 100.0),
        severity: Severity::of(thresholds.level(used)),
        text: format!("{shown}% {}", countdown(window.resets_at.map_or(0, |t| t - now))),
    }
}

fn empty_row(label: &'static str, text: &str) -> Row {
    Row { label, fill: None, severity: Severity::Ok, text: text.to_string() }
}

/// Fits in the text column next to the bar.
fn short_label(error: &FetchError) -> String {
    match error {
        FetchError::NoCredentials => "未登录".to_string(),
        FetchError::Expired | FetchError::Unauthorized => "已过期".to_string(),
        FetchError::CannotSave => "无法保存".to_string(),
        FetchError::RateLimited(_) => "限流".to_string(),
        FetchError::RefreshTemporary(_) => "续期待重试".to_string(),
        FetchError::WebCredentials | FetchError::WebOrganization => "网页配置".to_string(),
        FetchError::WebUnauthorized => "网页已过期".to_string(),
        FetchError::WebForbidden => "网页受阻".to_string(),
        FetchError::PreviousFailure => "待更新".to_string(),
        FetchError::Unsubscribed => "未订阅".to_string(),
        FetchError::Network(_) | FetchError::Offline(_) => "离线".to_string(),
        FetchError::Http(code) => format!("HTTP {code}"),
        FetchError::Parse => "错误".to_string(),
    }
}

/// Claude's desktop app keeps its own sign-in and does not touch the credential file this
/// program reads, so opening it does not help; signing in the command-line client does.
fn relogin(provider: Provider, what: &str) -> String {
    match provider {
        Provider::Claude => format!("{what}，请在终端运行 claude 并输入 /login 重新登录"),
        Provider::Codex => format!("{what}，请在 Codex 里重新登录"),
    }
}

/// One sentence for the tooltip and the detail panel.
pub fn describe(provider: Provider, error: &FetchError) -> String {
    let name = provider.name();
    match error {
        FetchError::NoCredentials => format!("未找到 {name} 的登录信息"),
        FetchError::Expired => relogin(provider, "登录已过期且无法自动续期"),
        FetchError::Unauthorized => relogin(provider, "登录凭据被拒绝"),
        FetchError::CannotSave => "凭据无法安全保存，请检查 Claude Code 的 .credentials.json 是否可写".to_string(),
        FetchError::RateLimited(_) => "请求过于频繁，稍后自动重试".to_string(),
        FetchError::RefreshTemporary(code) => format!("续期暂时失败（HTTP {code}），保留凭据，等待重试"),
        FetchError::WebCredentials => "请右键 → Claude 登录设置… 配置或更新网页登录".to_string(),
        FetchError::WebUnauthorized => "网页登录已失效，请更新登录信息".to_string(),
        FetchError::WebForbidden => "网页访问受阻（403），请在浏览器检查验证".to_string(),
        FetchError::WebOrganization => "请填写组织 ID：右键 → Claude 登录设置… → 高级设置".to_string(),
        FetchError::PreviousFailure => "上次请求失败，等待更新".to_string(),
        // Codex names its plan; for Claude it is inferred from an answer without windows.
        FetchError::Unsubscribed => match provider {
            Provider::Claude => "未返回任何额度（可能未订阅），已停止刷新".to_string(),
            Provider::Codex => "未订阅付费套餐，已停止刷新".to_string(),
        },
        FetchError::Network(_) | FetchError::Offline(_) => "网络不通，请检查代理".to_string(),
        FetchError::Http(code) => format!("服务返回 HTTP {code}"),
        FetchError::Parse => "返回内容无法识别".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;
    const T: Thresholds = Thresholds { warn: 75, crit: 90 };

    #[test]
    fn severity_follows_the_configured_thresholds() {
        let strict = Thresholds { warn: 3, crit: 95 };
        let view = build(Provider::Claude, Some(&usage()), None, false, strict, NOW);
        assert_eq!(view.rows[0].severity, Severity::Warn);
        assert_eq!(view.rows[1].severity, Severity::Warn);
    }

    fn usage() -> Usage {
        Usage {
            five_hour: Some(Window { used_pct: 4.4, resets_at: Some(NOW + 4 * 3600 + 10) }),
            seven_day: Some(Window { used_pct: 92.0, resets_at: Some(NOW + 86_400 + 10) }),
        }
    }

    #[test]
    fn shows_used_by_default() {
        let view = build(Provider::Claude, Some(&usage()), None, false, T, NOW);
        assert_eq!(view.rows[0].text, "4% 4h");
        assert_eq!(view.rows[0].fill, Some(0.04));
        assert_eq!(view.rows[0].severity, Severity::Ok);
        assert_eq!(view.rows[1].text, "92% 1d");
        assert_eq!(view.rows[1].severity, Severity::Crit);
        assert!(!view.stale);
        assert_eq!(view.tip, "IUsage · Claude\n5h 已用 4% · 4h 后重置\n7d 已用 92% · 1d 后重置");
    }

    #[test]
    fn remaining_mode_inverts_the_number_but_not_the_severity() {
        let view = build(Provider::Claude, Some(&usage()), None, true, T, NOW);
        assert_eq!(view.rows[1].text, "8% 1d");
        assert_eq!(view.rows[1].fill, Some(0.08));
        assert_eq!(view.rows[1].severity, Severity::Crit);
    }

    #[test]
    fn window_past_its_reset_reads_as_empty() {
        let view = build(Provider::Codex, Some(&usage()), None, false, T, NOW + 5 * 3600);
        assert_eq!(view.rows[0].text, "0% --");
        assert_eq!(view.rows[0].fill, Some(0.0));
        assert_eq!(view.rows[1].text, "92% 19h");
    }

    #[test]
    fn error_without_data_shows_a_status() {
        let view = build(Provider::Claude, None, Some(&FetchError::Expired), false, T, NOW);
        assert_eq!(view.rows[0].text, "已过期");
        assert_eq!(view.rows[0].fill, None);
        assert_eq!(view.rows[1].text, "--");
        assert!(!view.stale);
        assert!(view.tip.ends_with("登录已过期且无法自动续期，请在终端运行 claude 并输入 /login 重新登录"));
    }

    #[test]
    fn a_window_that_reset_while_the_fetch_was_failing_is_unknown_not_zero() {
        // Both windows reset after the last reading, and the refresh since has failed.
        let later = NOW + 2 * 86_400;
        let view = build(Provider::Claude, Some(&usage()), Some(&FetchError::Expired), false, T, later);
        assert_eq!(view.rows[0].text, "已过期");
        assert_eq!(view.rows[0].fill, None);
        assert_eq!(view.rows[1].text, "已过期");
        assert_eq!(view.rows[1].fill, None);
        assert!(view.stale);
        assert_eq!(view.tip, "IUsage · Claude\n登录已过期且无法自动续期，请在终端运行 claude 并输入 /login 重新登录");
        // A window that has not reset keeps its number, and the reset one is unknown.
        let view = build(Provider::Claude, Some(&usage()), Some(&FetchError::Http(503)), false, T, NOW + 5 * 3600);
        assert_eq!(view.rows[0].text, "HTTP 503");
        assert_eq!(view.rows[1].text, "92% 19h");
        assert_eq!(view.tip, "IUsage · Claude\n7d 已用 92% · 19h 后重置\n服务返回 HTTP 503");
        // Without an error the empty window is simply empty.
        let view = build(Provider::Claude, Some(&usage()), None, false, T, later);
        assert_eq!(view.rows[0].text, "0% --");
    }

    #[test]
    fn sign_in_advice_matches_where_the_credentials_live() {
        assert!(describe(Provider::Claude, &FetchError::Unauthorized).contains("/login"));
        assert!(describe(Provider::Codex, &FetchError::Unauthorized).contains("Codex"));
        assert!(describe(Provider::Claude, &FetchError::CannotSave).contains(".credentials.json"));
        let view = build(Provider::Claude, None, Some(&FetchError::CannotSave), false, T, NOW);
        assert_eq!(view.rows[0].text, "无法保存");
    }

    #[test]
    fn errors_are_told_apart() {
        let label = |e: FetchError| build(Provider::Claude, None, Some(&e), false, T, NOW).rows[0].text.clone();
        assert_eq!(label(FetchError::RateLimited(Some(60))), "限流");
        assert_eq!(label(FetchError::Http(503)), "HTTP 503");
        assert_eq!(label(FetchError::Network("x".into())), "离线");
        assert_eq!(label(FetchError::Parse), "错误");
        assert_eq!(label(FetchError::Unsubscribed), "未订阅");
    }

    #[test]
    fn a_free_codex_account_reads_as_unsubscribed_not_failed() {
        let view = build(Provider::Codex, None, Some(&FetchError::Unsubscribed), false, T, NOW);
        assert_eq!(view.rows[0].text, "未订阅");
        assert_eq!(view.rows[0].fill, None);
        assert!(!view.stale);
        assert_eq!(view.tip, "IUsage · Codex\n未订阅付费套餐，已停止刷新");
        let view = build(Provider::Claude, None, Some(&FetchError::Unsubscribed), false, T, NOW);
        assert_eq!(view.rows[0].text, "未订阅");
        assert_eq!(view.tip, "IUsage · Claude\n未返回任何额度（可能未订阅），已停止刷新");
    }

    #[test]
    fn error_with_earlier_data_is_stale() {
        let error = FetchError::Network("timeout".into());
        let view = build(Provider::Claude, Some(&usage()), Some(&error), false, T, NOW);
        assert!(view.stale);
        assert_eq!(view.rows[0].text, "4% 4h");
    }

    #[test]
    fn loading_state() {
        let view = build(Provider::Codex, None, None, false, T, NOW);
        assert_eq!(view.rows[0].text, "…");
        assert_eq!(view.tip, "IUsage · Codex");
    }

    #[test]
    fn repaint_depends_on_pixels_not_tooltip() {
        let original = build(Provider::Codex, Some(&usage()), None, false, T, NOW);
        let mut changed = original.clone();
        changed.tip.push_str(" updated timestamp");
        assert!(original.same_pixels(&changed));
        changed.rows[0].text.push('!');
        assert!(!original.same_pixels(&changed));
        changed = original.clone();
        changed.stale = true;
        assert!(!original.same_pixels(&changed));
        changed = original.clone();
        changed.provider = Provider::Claude;
        assert!(!original.same_pixels(&changed));
    }
}
