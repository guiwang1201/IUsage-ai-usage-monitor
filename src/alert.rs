//! Decides when usage is worth a notification: once per reminder point per window.

use crate::model::Usage;
use crate::timefmt::countdown;
use crate::view::used_now;

/// Remaining percentages that each get one notification per window, highest first.
pub const REMIND_AT: [i64; 3] = [20, 10, 5];

/// How far below a reminder point usage must fall before that point can fire again.
/// Usage only drops this much when a window resets, so a reading that wobbles around
/// a point does not notify twice.
const REARM_MARGIN: i64 = 5;

/// Number of reminder points already announced for the 5-hour and the 7-day window
/// (0 none .. 3 all).
pub type Announced = [u8; 2];

/// How many reminder points a used percentage has passed.
fn level(used: i64) -> u8 {
    REMIND_AT.iter().filter(|&&left| 100 - used <= left).count() as u8
}

/// A window whose previous reset time has passed is a new window, and it can already
/// be above a reminder point (e.g. after sleep). The services report a reset time that
/// moves by a second or so between readings, so a later time alone is not a reset.
pub fn rearm(announced: &mut Announced, previous: &Usage, current: &Usage, now: i64) {
    for ((slot, before), after) in announced.iter_mut()
        .zip([previous.five_hour, previous.seven_day])
        .zip([current.five_hour, current.seven_day])
    {
        if before.and_then(|w| w.resets_at).is_some_and(|old| old <= now) && after.is_some() {
            *slot = 0;
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Alert {
    pub critical: bool,
    /// One line per window that just passed a reminder point.
    pub lines: Vec<String>,
}

/// Updates `announced` for a fresh reading and returns what should be notified, if anything.
pub fn check(announced: &mut Announced, usage: &Usage, now: i64) -> Option<Alert> {
    let mut alert = Alert { critical: false, lines: Vec::new() };
    let windows = [("5 小时", usage.five_hour), ("7 天", usage.seven_day)];
    for (slot, (name, window)) in announced.iter_mut().zip(windows) {
        let used = window.map_or(0, |w| used_now(&w, now));
        *slot = (*slot).min(level(used + REARM_MARGIN));
        let reached = level(used);
        if reached > *slot {
            *slot = reached;
            alert.critical |= reached >= 2;
            let mut line = format!("{name}额度剩余 {}%", 100 - used);
            if let Some(left) = window.and_then(|w| w.resets_at).map(|t| t - now).filter(|secs| *secs > 0) {
                line.push_str(&format!("，{} 后重置", countdown(left)));
            }
            alert.lines.push(line);
        }
    }
    (!alert.lines.is_empty()).then_some(alert)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Window;

    const NOW: i64 = 1_000_000;

    fn usage(five_hour: f64, seven_day: f64) -> Usage {
        Usage {
            five_hour: Some(Window { used_pct: five_hour, resets_at: Some(NOW + 2 * 3600 + 30) }),
            seven_day: Some(Window { used_pct: seven_day, resets_at: Some(NOW + 3 * 86_400 + 30) }),
        }
    }

    #[test]
    fn quiet_above_twenty_percent_left() {
        let mut announced = [0, 0];
        assert_eq!(check(&mut announced, &usage(79.0, 10.0), NOW), None);
        assert_eq!(announced, [0, 0]);
    }

    #[test]
    fn three_reminders_per_window_and_no_more() {
        let mut announced = [0, 0];
        let alert = check(&mut announced, &usage(80.0, 10.0), NOW).unwrap();
        assert_eq!(alert, Alert { critical: false, lines: vec!["5 小时额度剩余 20%，2.0h 后重置".to_string()] });
        for used in [81.0, 85.0, 89.0] {
            assert_eq!(check(&mut announced, &usage(used, 10.0), NOW), None);
        }
        let alert = check(&mut announced, &usage(90.0, 10.0), NOW).unwrap();
        assert!(alert.critical);
        assert_eq!(check(&mut announced, &usage(93.0, 10.0), NOW), None);
        assert!(check(&mut announced, &usage(95.0, 10.0), NOW).is_some());
        for used in [96.0, 99.0, 100.0] {
            assert_eq!(check(&mut announced, &usage(used, 10.0), NOW), None);
        }
        assert_eq!(announced, [3, 0]);
    }

    #[test]
    fn jumping_past_several_points_is_one_alert() {
        let mut announced = [0, 0];
        let alert = check(&mut announced, &usage(10.0, 96.0), NOW).unwrap();
        assert_eq!(alert, Alert { critical: true, lines: vec!["7 天额度剩余 4%，3.0d 后重置".to_string()] });
        assert_eq!(announced, [0, 3]);
    }

    #[test]
    fn both_windows_share_one_alert() {
        let mut announced = [0, 0];
        let alert = check(&mut announced, &usage(82.0, 92.0), NOW).unwrap();
        assert_eq!(alert.lines.len(), 2);
        assert!(alert.critical);
    }

    #[test]
    fn a_reset_rearms_the_window() {
        let mut announced = [3, 0];
        assert_eq!(check(&mut announced, &usage(3.0, 10.0), NOW), None);
        assert_eq!(announced, [0, 0]);
        assert!(check(&mut announced, &usage(80.0, 10.0), NOW).is_some());
    }

    #[test]
    fn wobbling_around_a_point_does_not_repeat() {
        let mut announced = [0, 0];
        assert!(check(&mut announced, &usage(80.0, 10.0), NOW).is_some());
        assert_eq!(check(&mut announced, &usage(79.0, 10.0), NOW), None);
        assert_eq!(check(&mut announced, &usage(80.0, 10.0), NOW), None);
    }

    #[test]
    fn a_window_past_its_reset_counts_as_empty() {
        let mut announced = [0, 0];
        assert_eq!(check(&mut announced, &usage(95.0, 10.0), NOW + 3 * 3600), None);
    }

    #[test]
    fn a_reset_time_that_drifts_later_does_not_rearm() {
        let old = usage(85.0, 10.0);
        let mut announced = [0, 0];
        assert!(check(&mut announced, &old, NOW).is_some());
        for drift in 1..=5 {
            let mut new = old;
            new.five_hour.as_mut().unwrap().resets_at = Some(NOW + 2 * 3600 + 30 + drift);
            rearm(&mut announced, &old, &new, NOW + drift * 90);
            assert_eq!(check(&mut announced, &new, NOW + drift * 90), None);
        }
        assert_eq!(announced, [1, 0]);
    }

    #[test]
    fn a_new_window_can_already_be_high_when_resuming() {
        let old = usage(95.0, 95.0);
        let mut new = old;
        new.five_hour.as_mut().unwrap().resets_at = Some(NOW + 8 * 3600);
        let later = NOW + 2 * 3600 + 60;
        let mut announced = [3, 3];
        rearm(&mut announced, &old, &new, later);
        assert_eq!(announced, [0, 3]);
        assert_eq!(check(&mut announced, &new, later).unwrap().lines.len(), 1);
    }
}
