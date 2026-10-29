//! Configuration file support matching N_m3u8DL-RE `config.conf`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Locate default config file path for the current OS.
pub fn default_config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Ok(appdata) = env::var("APPDATA") {
            let p = PathBuf::from(appdata).join("N_m3u8DL-RE").join("config.conf");
            return Some(p);
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
            let p = PathBuf::from(xdg);
            if p.is_absolute() {
                return Some(p.join("N_m3u8DL-RE").join("config.conf"));
            }
        }
        if let Ok(home) = env::var("HOME") {
            let p = PathBuf::from(home).join(".config").join("N_m3u8DL-RE").join("config.conf");
            return Some(p);
        }
    }
    None
}

/// Parse configuration file lines into command-line argument tokens.
pub fn parse_config_file(path: &Path) -> Result<Vec<String>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path).map_err(|e| format!("cannot read config file '{}': {e}", path.display()))?;
    let mut args = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Tokenize line respecting quotes
        let tokens = tokenize_line(trimmed);
        args.extend(tokens);
    }

    Ok(args)
}

fn tokenize_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = '\0';
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quote != '\0' {
            if c == in_quote {
                in_quote = '\0';
            } else {
                cur.push(c);
            }
        } else if c == '"' || c == '\'' {
            in_quote = c;
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                tokens.push(cur.clone());
                cur.clear();
            }
        } else {
            cur.push(c);
        }
    }

    if !cur.is_empty() {
        tokens.push(cur);
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write;

    #[test]
    fn test_parse_config() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "# Global configuration").unwrap();
        writeln!(f, "--auto-select").unwrap();
        writeln!(f, "--thread-count 16").unwrap();
        writeln!(f, "--save-dir \"Downloads/My Videos\"").unwrap();
        f.flush().unwrap();

        let args = parse_config_file(f.path()).unwrap();
        assert_eq!(
            args,
            vec![
                "--auto-select",
                "--thread-count",
                "16",
                "--save-dir",
                "Downloads/My Videos",
            ]
        );
    }
}
