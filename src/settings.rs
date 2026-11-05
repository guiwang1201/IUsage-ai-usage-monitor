//! User settings, stored as a small JSON file.

use std::path::Path;

use tinyjson::JsonValue;

use crate::json;
use crate::model::{Provider, Thresholds};

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    /// None keeps compatibility with the legacy environment configuration.
    pub claude_web: Option<bool>,
    /// Which assistant the widget shows.
    pub provider: Provider,
    /// Show what is left instead of what has been used.
    pub show_remaining: bool,
    /// How far left of the tray the widget sits, in 96-dpi pixels.
    pub offset_x: i32,
    pub poll_secs: u32,
    /// Send a notification when usage crosses a threshold.
    pub notify: bool,
    /// Editable in the file only; there is no menu for it.
    pub thresholds: Thresholds,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            claude_web: None,
            provider: Provider::Claude,
            show_remaining: false,
            offset_x: 0,
            // Keep the existing per-provider request floor and server cooldowns.
            poll_secs: crate::schedule::MIN_GAP_SECS as u32,
            notify: true,
            thresholds: Thresholds::default(),
        }
    }
}

impl Settings {
    /// Missing or malformed files and fields fall back to defaults.
    pub fn load(path: &Path) -> Settings {
        let defaults = Settings::default();
        let Ok(text) = json::read_small_file(path) else { return defaults };
        let Some(JsonValue::Object(map)) = json::parse(&text) else { return defaults };
        let number = |key: &str| match map.get(key) {
            Some(JsonValue::Number(n)) => Some(*n),
            _ => None,
        };
        let crit = number("crit_pct").map_or(defaults.thresholds.crit, |n| (n as i64).clamp(1, 100));
        // A warning level above the critical one would never be seen.
        let warn = number("warn_pct").map_or(defaults.thresholds.warn, |n| (n as i64).clamp(1, 100)).min(crit);
        Settings {
            claude_web: match map.get("claude_web") { Some(JsonValue::Boolean(v)) => Some(*v), _ => None },
            provider: match map.get("provider") {
                Some(JsonValue::String(key)) => Provider::from_key(key).unwrap_or(defaults.provider),
                _ => defaults.provider,
            },
            show_remaining: matches!(map.get("show_remaining"), Some(JsonValue::Boolean(true))),
            offset_x: number("offset_x").map_or(defaults.offset_x, |n| (n as i32).max(0)),
            poll_secs: number("poll_secs").map_or(defaults.poll_secs, |n| (n as u32).clamp(crate::schedule::MIN_GAP_SECS as u32, 3600)),
            notify: !matches!(map.get("notify"), Some(JsonValue::Boolean(false))),
            thresholds: Thresholds { warn, crit },
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let fields = [
            format!("\"claude_web\": {}", self.claude_web.map_or("null", |v| if v { "true" } else { "false" })),
            format!("\"provider\": \"{}\"", self.provider.key()),
            format!("\"show_remaining\": {}", self.show_remaining),
            format!("\"offset_x\": {}", self.offset_x),
            format!("\"poll_secs\": {}", self.poll_secs),
            format!("\"notify\": {}", self.notify),
            format!("\"warn_pct\": {}", self.thresholds.warn),
            format!("\"crit_pct\": {}", self.thresholds.crit),
        ];
        json::write_file(path, &format!("{{\n  {}\n}}\n", fields.join(",\n  ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let path = std::env::temp_dir().join(format!("iusage-settings-test-{}.json", std::process::id()));
        let settings = Settings {
            claude_web: Some(true),
            provider: Provider::Codex,
            show_remaining: true,
            offset_x: 180,
            poll_secs: 300,
            notify: false,
            thresholds: Thresholds { warn: 60, crit: 85 },
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_file_saved_with_a_byte_order_mark_still_loads() {
        let path = std::env::temp_dir().join(format!("iusage-settings-bom-{}.json", std::process::id()));
        std::fs::write(&path, "\u{feff}{\"provider\":\"codex\",\"warn_pct\":60}").unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.provider, Provider::Codex);
        assert_eq!(loaded.thresholds.warn, 60);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn falls_back_to_defaults() {
        let path = std::env::temp_dir().join(format!("iusage-settings-bad-{}.json", std::process::id()));
        assert_eq!(Settings::load(&path), Settings::default());
        std::fs::write(&path, r#"{"provider":"gemini","poll_secs":1,"offset_x":-40,"warn_pct":95,"crit_pct":80}"#).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.provider, Provider::Claude);
        assert_eq!(loaded.poll_secs, 50);
        assert_eq!(loaded.offset_x, 0);
        assert!(loaded.notify, "a file written before the option existed keeps notifications on");
        assert_eq!(loaded.thresholds, Thresholds { warn: 80, crit: 80 });
        std::fs::remove_file(&path).unwrap();
    }
}
