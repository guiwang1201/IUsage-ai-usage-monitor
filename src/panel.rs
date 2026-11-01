//! Content of the detail panel: both providers, exact reset times, and how old the
//! numbers are. Platform-independent.

use crate::cache::{Cache, Entry};
use crate::model::{FetchError, Provider, Thresholds, Window};
use crate::timefmt::{age, clock, countdown_long, weekday_clock};
use crate::view::{describe, reset_since, used_now, Severity};

#[derive(Clone, PartialEq, Debug)]
pub struct Row {
    pub name: &'static str,
    /// `已用 23%`, `剩余 77%`, or `--` without data.
    pub value: String,
    /// Filled share of the bar, 0..=1. `None` draws an empty bar.
    pub fill: Option<f32>,
    pub severity: Severity,
    /// When the window resets.
    pub detail: String,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Section {
    pub provider: Provider,
    /// This is the provider the taskbar widget is showing.
    pub current: bool,
    pub rows: [Row; 2],
    /// How fresh the numbers are, or what went wrong.
    pub status: String,
    pub provenance: String,
    /// Whether the quota should last until it resets, judged from how fast it is used.
    pub pace: String,
    /// The pace is a warning rather than reassurance.
    pub pace_warn: bool,
    pub retry: String,
    pub update_login: bool,
    pub failed: bool,
    /// Codex on a free plan: shown as a short notice instead of bars, and not refreshed.
    pub unsubscribed: bool,
}

pub type Panel = [Section; 2];

pub struct Context {
    pub current: Provider,
    pub show_remaining: bool,
    pub thresholds: Thresholds,
    pub now: i64,
    /// Local time zone, in seconds east of UTC.
    pub utc_offset: i64,
}

pub fn build(cache: &Cache, errors: &[Option<FetchError>; 2], ctx: &Context) -> Panel {
    Provider::ALL.map(|provider| {
        let i = provider.index();
        section(provider, cache[i].as_ref(), errors[i].as_ref(), ctx)
    })
}

fn section(provider: Provider, entry: Option<&Entry>, error: Option<&FetchError>, ctx: &Context) -> Section {
    let usage = entry.map(|e| e.usage).unwrap_or_default();
    let unsubscribed = matches!(error, Some(FetchError::Unsubscribed));
    let status = match (entry, error) {
        _ if unsubscribed => describe(provider, &FetchError::Unsubscribed),
        (None, None) => "正在获取…".to_string(),
        (None, Some(error)) => describe(provider, error),
        (Some(entry), None) => match age(ctx.now - entry.fetched_at).as_str() {
            "刚刚" => "刚刚更新".to_string(),
            ago => format!("{ago}更新"),
        },
        (Some(entry), Some(error)) => {
            format!("{}（{}的数据）", describe(provider, error), age(ctx.now - entry.fetched_at))
        }
    };
    // Only judged from a fresh reading: after a failure the numbers may be out of date.
    let (pace, pace_warn) = match error {
        None => pace(&usage, ctx),
        Some(_) => (String::new(), false),
    };
    Section {
        provider,
        current: provider == ctx.current,
        rows: [row("5 小时", usage.five_hour, error.is_some(), ctx), row("7 天", usage.seven_day, error.is_some(), ctx)],
        status,
        provenance: entry.map_or_else(|| "尚无成功记录".into(), |e| format!("{} · 上次成功 {}", e.source.label(), clock(e.fetched_at, ctx.now, ctx.utc_offset))),
        pace,
        pace_warn,
        retry: String::new(),
        update_login: provider == Provider::Claude && matches!(error, Some(FetchError::WebUnauthorized | FetchError::WebCredentials | FetchError::WebOrganization)),
        failed: error.is_some() && !unsubscribed,
        unsubscribed,
    }
}

/// How a window is being used, worst last.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Pace {
    /// On course to use at most `PACE_TIGHT` percent before it resets.
    Steady,
    /// On course to use most of it.
    Tight,
    /// On course to run out at the given moment, before it resets.
    RunsOut(i64),
    Exhausted,
}

impl Pace {
    /// Larger is worse; of two windows running out, the sooner one is worse.
    fn rank(self) -> (u8, i64) {
        match self {
            Pace::Steady => (0, 0),
            Pace::Tight => (1, 0),
            Pace::RunsOut(at) => (2, -at),
            Pace::Exhausted => (3, 0),
        }
    }
}

/// Projected use above this share counts as tight.
const PACE_TIGHT: f64 = 90.0;
/// Too early in a window to judge: the first tenth of it.
const PACE_MIN_ELAPSED: f64 = 0.1;

fn pace_of(window: Option<Window>, length: i64, now: i64) -> Option<(Pace, i64)> {
    let window = window?;
    let reset = window.resets_at.filter(|r| *r > now)?;
    let used = window.used_pct.clamp(0.0, 100.0);
    if used >= 100.0 {
        return Some((Pace::Exhausted, reset));
    }
    let elapsed = length - (reset - now);
    if (elapsed as f64) < length as f64 * PACE_MIN_ELAPSED || elapsed > length {
        return None;
    }
    let rate = used / elapsed as f64; // percent per second
    let projected = used + rate * (reset - now) as f64;
    let pace = if projected > 100.0 {
        Pace::RunsOut(now + ((100.0 - used) / rate) as i64)
    } else if projected > PACE_TIGHT {
        Pace::Tight
    } else {
        Pace::Steady
    };
    Some((pace, reset))
}

/// One line on whether the quota will last, from the window that looks worse; the
/// weekly one when they look the same.
fn pace(usage: &crate::model::Usage, ctx: &Context) -> (String, bool) {
    let weekly = pace_of(usage.seven_day, 7 * 86_400, ctx.now).map(|p| ("", p));
    let session = pace_of(usage.five_hour, 5 * 3600, ctx.now).map(|p| ("5 小时", p));
    let worse = match (weekly, session) {
        (Some(w), Some(s)) if s.1 .0.rank() > w.1 .0.rank() => Some(s),
        (None, s) => s,
        (w, _) => w,
    };
    let Some((name, (pace, reset))) = worse else {
        return (String::new(), false);
    };
    let when = |t| weekday_clock(t, ctx.now, ctx.utc_offset);
    match pace {
        Pace::Steady => (format!("{name}进度正常，{} 重置前额度充裕", when(reset)), false),
        Pace::Tight => (format!("{name}进度偏快，{} 重置前可能接近用完", when(reset)), true),
        Pace::RunsOut(at) => (format!("{name}额度用得较快，预计 {} 用完", when(at)), true),
        Pace::Exhausted => (format!("{name}额度已用完，{} 重置", when(reset)), true),
    }
}

fn row(name: &'static str, window: Option<Window>, failed: bool, ctx: &Context) -> Row {
    let Some(window) = window else {
        return Row { name, value: "--".to_string(), fill: None, severity: Severity::Ok, detail: "暂无数据".to_string() };
    };
    // After a failed fetch a window that has reset since is not known to be empty.
    if failed && reset_since(&window, ctx.now) {
        return Row {
            name,
            value: "--".to_string(),
            fill: None,
            severity: Severity::Ok,
            detail: "已重置，当前用量未知".to_string(),
        };
    }
    let used = used_now(&window, ctx.now);
    let (what, shown) = if ctx.show_remaining { ("剩余", 100 - used) } else { ("已用", used) };
    let detail = match window.resets_at {
        Some(reset) if reset > ctx.now => {
            format!("{} 后重置 · {}", countdown_long(reset - ctx.now), clock(reset, ctx.now, ctx.utc_offset))
        }
        Some(_) => "已重置".to_string(),
        None => "重置时间未知".to_string(),
    };
    Row {
        name,
        value: format!("{what} {shown}%"),
        fill: Some(shown as f32 / 100.0),
        severity: Severity::of(ctx.thresholds.level(used)),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_login_keeps_old_quota_and_offers_update_only_for_login_errors() {
        let panel = build(&[Some(claude()), None], &[Some(FetchError::WebUnauthorized), None], &ctx());
        assert!(panel[0].update_login);
        assert!(panel[0].failed);
        assert_eq!(panel[0].rows[0].value, "已用 23%");
        for error in [FetchError::WebForbidden, FetchError::RateLimited(Some(300)), FetchError::Network("timeout".into())] {
            let panel = build(&[Some(claude()), None], &[Some(error), None], &ctx());
            assert!(!panel[0].update_login);
        }
    }
    use crate::model::Usage;

    const NOW: i64 = 1_790_860_000; // 21:06 on 1 October 2026 in UTC+8

    fn ctx() -> Context {
        Context {
            current: Provider::Claude,
            show_remaining: false,
            thresholds: Thresholds::default(),
            now: NOW,
            utc_offset: 8 * 3600,
        }
    }

    fn claude() -> Entry {
        Entry {
            source: crate::cache::Source::OAuth,
            fetched_at: NOW - 200,
            announced: [0, 0],
            usage: Usage {
                five_hour: Some(Window { used_pct: 23.0, resets_at: Some(1_790_875_800) }),
                seven_day: Some(Window { used_pct: 91.0, resets_at: Some(1_791_108_875) }),
            },
        }
    }

    #[test]
    fn shows_both_providers_with_exact_reset_times() {
        let panel = build(&[Some(claude()), None], &[None, None], &ctx());

        let c = &panel[0];
        assert!(c.current && !c.failed);
        assert_eq!(c.rows[0].value, "已用 23%");
        assert_eq!(c.rows[0].detail, "4 小时 23 分 后重置 · 明天 01:30");
        assert_eq!(c.rows[1].detail, "2 天 21 小时 后重置 · 10月4日 18:14");
        assert_eq!(c.rows[1].severity, Severity::Crit);
        assert_eq!(c.status, "3 分钟前更新");

        let x = &panel[1];
        assert_eq!(x.provider, Provider::Codex);
        assert!(!x.current);
        assert_eq!(x.rows[0].value, "--");
        assert_eq!(x.rows[0].fill, None);
        assert_eq!(x.status, "正在获取…");
    }

    #[test]
    fn a_free_codex_account_is_a_notice_not_a_failure() {
        let mut stale = claude();
        stale.source = crate::cache::Source::Codex;
        let panel = build(&[Some(claude()), Some(stale)], &[None, Some(FetchError::Unsubscribed)], &ctx());
        assert!(panel[1].unsubscribed);
        assert!(!panel[1].failed && !panel[1].update_login);
        assert_eq!(panel[1].status, "未订阅付费套餐，已停止刷新");
        assert!(!panel[0].unsubscribed);
    }

    #[test]
    fn remaining_mode() {
        let panel = build(&[Some(claude()), None], &[None, None], &Context { show_remaining: true, ..ctx() });
        assert_eq!(panel[0].rows[0].value, "剩余 77%");
        assert_eq!(panel[0].rows[0].fill, Some(0.77));
    }

    #[test]
    fn errors_are_explained_and_old_data_is_dated() {
        let errors = [Some(FetchError::Network("x".into())), Some(FetchError::NoCredentials)];
        let panel = build(&[Some(claude()), None], &errors, &ctx());
        assert_eq!(panel[0].status, "网络不通，请检查代理（3 分钟前的数据）");
        assert!(panel[0].failed);
        assert_eq!(panel[1].status, "未找到 Codex 的登录信息");
    }

    #[test]
    fn fresh_and_reset_states() {
        let mut entry = claude();
        entry.fetched_at = NOW - 5;
        entry.usage.five_hour = Some(Window { used_pct: 80.0, resets_at: Some(NOW - 10) });
        entry.usage.seven_day = Some(Window { used_pct: 10.0, resets_at: None });
        let panel = build(&[Some(entry), None], &[None, None], &ctx());
        assert_eq!(panel[0].status, "刚刚更新");
        assert_eq!(panel[0].rows[0].value, "已用 0%");
        assert_eq!(panel[0].rows[0].detail, "已重置");
        assert_eq!(panel[0].rows[1].detail, "重置时间未知");
    }

    fn usage(five: Option<(f64, i64)>, seven: Option<(f64, i64)>) -> Usage {
        let w = |(used_pct, left): (f64, i64)| Window { used_pct, resets_at: Some(NOW + left) };
        Usage { five_hour: five.map(w), seven_day: seven.map(w) }
    }

    fn pace_line(u: Usage) -> (String, bool) {
        pace(&u, &ctx())
    }

    #[test]
    fn pace_follows_the_weekly_window() {
        const DAY: i64 = 86_400;
        // Two of seven days gone (Thursday 21:06), reset on Tuesday 21:06.
        assert_eq!(pace_line(usage(None, Some((20.0, 5 * DAY)))), ("进度正常，星期二 21:06 重置前额度充裕".into(), false));
        assert_eq!(pace_line(usage(None, Some((27.0, 5 * DAY)))), ("进度偏快，星期二 21:06 重置前可能接近用完".into(), true));
        // 24% after two days and an hour heads for about 82%, which claude.ai also calls normal.
        assert_eq!(pace_line(usage(None, Some((24.0, 4 * DAY + 23 * 3600)))).1, false);
        // 40% in two days leaves three more days at that rate: Sunday.
        assert_eq!(pace_line(usage(None, Some((40.0, 5 * DAY)))), ("额度用得较快，预计 星期日 21:06 用完".into(), true));
        assert_eq!(pace_line(usage(None, Some((100.0, 5 * DAY)))), ("额度已用完，星期二 21:06 重置".into(), true));
    }

    #[test]
    fn pace_waits_for_enough_of_the_window_and_skips_unknown_resets() {
        const DAY: i64 = 86_400;
        // Twelve hours into the week is too early to judge, even at 30%.
        assert_eq!(pace_line(usage(None, Some((30.0, 7 * DAY - 12 * 3600)))).0, "");
        assert_eq!(pace_line(usage(None, None)).0, "");
        assert_eq!(pace_line(Usage { five_hour: None, seven_day: Some(Window { used_pct: 50.0, resets_at: None }) }).0, "");
        assert_eq!(pace_line(usage(None, Some((50.0, -10)))).0, "");
    }

    #[test]
    fn a_tighter_five_hour_window_is_named() {
        const DAY: i64 = 86_400;
        // Three of five hours gone at 90%: runs out in 20 minutes, at 21:26.
        let line = pace_line(usage(Some((90.0, 2 * 3600)), Some((10.0, 5 * DAY))));
        assert_eq!(line, ("5 小时额度用得较快，预计 今天 21:26 用完".into(), true));
        // Too early in the week to judge it, so the five-hour window speaks.
        let line = pace_line(usage(Some((55.0, 2 * 3600)), Some((10.0, 7 * DAY - 3600))));
        assert_eq!(line, ("5 小时进度偏快，今天 23:06 重置前可能接近用完".into(), true));
        // Steady on both: the weekly one speaks.
        assert_eq!(pace_line(usage(Some((10.0, 2 * 3600)), Some((10.0, 5 * DAY)))).0, "进度正常，星期二 21:06 重置前额度充裕");
    }

    #[test]
    fn no_pace_while_the_fetch_is_failing() {
        let panel = build(&[Some(claude()), None], &[Some(FetchError::Network("x".into())), None], &ctx());
        assert_eq!(panel[0].pace, "");
        let panel = build(&[Some(claude()), None], &[None, None], &ctx());
        assert!(panel[0].pace_warn, "91% of the week used with almost three days left");
    }

    #[test]
    fn a_reset_window_is_unknown_while_the_fetch_is_failing() {
        let mut entry = claude();
        entry.usage.five_hour = Some(Window { used_pct: 80.0, resets_at: Some(NOW - 10) });
        let errors = [Some(FetchError::Expired), None];
        let panel = build(&[Some(entry), None], &errors, &ctx());
        assert_eq!(panel[0].rows[0].value, "--");
        assert_eq!(panel[0].rows[0].fill, None);
        assert_eq!(panel[0].rows[0].detail, "已重置，当前用量未知");
        // The window that has not reset keeps its number.
        assert_eq!(panel[0].rows[1].value, "已用 91%");
    }
}
