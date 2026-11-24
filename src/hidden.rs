//! Message types the user hid from the Summary dashboard.
//!
//! One entry per line in a small text file: a message type (as shown in the
//! summary) or any part of one. A type that contains an entry is left out of
//! the counts and lists. Lines starting with # are comments.
//!
//! Location: $AXTAIL_HIDDEN, else %APPDATA%\axtail\hidden.txt on Windows,
//! else $XDG_CONFIG_HOME/axtail/hidden.txt or ~/.config/axtail/hidden.txt.

use std::fs;
use std::io;
use std::path::PathBuf;

const HEADER: &str = "\
# axtail: message types hidden from the Summary dashboard (Ctrl+D).
# One per line. A type that contains a line's text is hidden.
# Delete a line to show that type again.
";

pub fn path() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(p) = var("AXTAIL_HIDDEN") {
        return Some(PathBuf::from(p));
    }
    let dir = if cfg!(windows) {
        PathBuf::from(var("APPDATA")?)
    } else if let Some(x) = var("XDG_CONFIG_HOME") {
        PathBuf::from(x)
    } else {
        PathBuf::from(var("HOME")?).join(".config")
    };
    Some(dir.join("axtail").join("hidden.txt"))
}

pub fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// The saved entries; none when the file does not exist yet.
pub fn load() -> Vec<String> {
    path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map_or_else(Vec::new, |t| parse(&t))
}

fn no_path() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no home or config folder")
}

/// Appends an entry, creating the file when needed.
pub fn add(entry: &str) -> io::Result<PathBuf> {
    let p = path().ok_or_else(no_path)?;
    let mut text = fs::read_to_string(&p).unwrap_or_else(|_| HEADER.to_string());
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(entry.trim());
    text.push('\n');
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&p, text)?;
    Ok(p)
}

/// Removes an entry, keeping every other line as it was.
pub fn remove(entry: &str) -> io::Result<PathBuf> {
    let p = path().ok_or_else(no_path)?;
    let text = fs::read_to_string(&p)?;
    let kept: String = text
        .lines()
        .filter(|l| l.trim() != entry.trim())
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(&p, kept)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_blanks_are_skipped() {
        assert_eq!(
            parse("# note\n\n  Picked up JAVA_TOOL_OPTIONS  \nfoo <n>\n"),
            vec!["Picked up JAVA_TOOL_OPTIONS", "foo <n>"]
        );
    }
}
