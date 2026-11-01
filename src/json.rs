//! JSON reading shared by everything that parses a file or a response.

use tinyjson::JsonValue;

/// Settings, cache and credentials are small. Bound reads even if a file grows
/// while open, so an accidentally replaced file cannot consume arbitrary memory.
pub fn read_small_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::{Read, Error, ErrorKind};
    const LIMIT: u64 = 1024 * 1024;
    let mut text = String::new();
    std::fs::File::open(path)?.take(LIMIT + 1).read_to_string(&mut text)?;
    if text.len() as u64 > LIMIT {
        return Err(Error::new(ErrorKind::InvalidData, "Local JSON file exceeds size limit"));
    }
    Ok(text)
}

/// Replace a complete file so a crash cannot leave a truncated settings/cache file.
pub fn write_file(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let temporary = path.with_file_name(name);
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
            let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
            let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(windows))]
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// Deepest nesting accepted. The parser recurses once per level, so unbounded nesting
/// in a file or a response would overflow the stack and take the process down.
const MAX_DEPTH: usize = 64;

/// Parses `text`, or returns `None` if it is not JSON or is nested too deeply.
/// A leading byte-order mark, which some editors add when saving, is ignored.
pub fn parse(text: &str) -> Option<JsonValue> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if too_deep(text) {
        return None;
    }
    text.parse().ok()
}

fn too_deep(text: &str) -> bool {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for byte in text.bytes() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

pub fn get<'a>(value: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    match value {
        JsonValue::Object(map) => map.get(key),
        _ => None,
    }
}

pub fn num(value: &JsonValue) -> Option<f64> {
    match value {
        JsonValue::Number(n) if n.is_finite() => Some(*n),
        _ => None,
    }
}

pub fn text(value: &JsonValue) -> Option<&str> {
    match value {
        JsonValue::String(s) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_file_reads_are_bounded() {
        let path = std::env::temp_dir().join(format!("iusage-bounded-{}.json", std::process::id()));
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(read_small_file(&path).unwrap(), "{}");
        std::fs::write(&path, vec![b' '; 1024 * 1024]).unwrap();
        assert_eq!(read_small_file(&path).unwrap().len(), 1024 * 1024);
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert_eq!(read_small_file(&path).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn parses_ordinary_json_with_or_without_a_bom() {
        assert_eq!(parse(r#"{"a":1}"#).and_then(|v| get(&v, "a").and_then(num)), Some(1.0));
        assert_eq!(parse("\u{feff}{\"a\":\"b\"}").and_then(|v| get(&v, "a").and_then(text).map(String::from)), Some("b".to_string()));
    }

    #[test]
    fn rejects_nesting_that_would_overflow_the_stack() {
        assert!(parse(&"[".repeat(200_000)).is_none());
        assert!(parse(&"{\"a\":".repeat(100_000)).is_none());
        let balanced = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(parse(&balanced).is_none());
        let allowed = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(parse(&allowed).is_some());
    }

    #[test]
    fn brackets_inside_strings_do_not_count() {
        let value = format!("{{\"k\":\"{}\\\"{}\"}}", "[".repeat(500), "{".repeat(500));
        assert!(parse(&value).is_some());
    }

    #[test]
    fn malformed_input_is_rejected_without_panicking() {
        for bad in [
            "", " ", "{", "}", "[1,", "{\"a\"}", "{\"a\":}", "nul", "\"unterminated", "\"\\u12\"", "\"\\uD800\"",
            "1e999999", "-", "{\"a\":1,}", "\u{0}", "{\"a\":1}{\"b\":2}", "<html></html>", "[1 2]", "\"\\x\"",
        ] {
            let _ = parse(bad);
        }
    }
}
