//! The `doctor` command's report: this system and the outside tools it finds (ffmpeg, mkvmerge, decryptors).

use std::env;
use std::path::PathBuf;
use std::process::Command;
use serde::{Deserialize, Serialize};

/// Status and version of an external utility tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStatus {
    pub name: String,
    pub status: String,
    pub path: Option<String>,
    pub version: Option<String>,
    pub required: bool,
}

/// Overall doctor environment report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub os: String,
    pub arch: String,
    pub tools: Vec<ToolStatus>,
}

impl DoctorReport {
    /// Perform the full inspection.
    pub fn inspect(
        custom_ffmpeg: Option<&str>,
        custom_mkvmerge: Option<&str>,
        custom_decrypt: Option<&str>,
    ) -> Self {
        let os = env::consts::OS.to_string();
        let arch = env::consts::ARCH.to_string();

        let mut tools = Vec::new();

        // 1. FFmpeg
        tools.push(check_tool(
            "ffmpeg",
            custom_ffmpeg,
            &["-version"],
            true,
        ));

        // 2. mkvmerge
        tools.push(check_tool(
            "mkvmerge",
            custom_mkvmerge,
            &["--version"],
            false,
        ));

        // 3. mp4decrypt
        tools.push(check_tool(
            "mp4decrypt",
            custom_decrypt,
            &["--version"],
            false,
        ));

        // 4. shaka-packager / packager
        tools.push(check_tool(
            "shaka-packager",
            None,
            &["--version"],
            false,
        ));

        Self {
            os,
            arch,
            tools,
        }
    }
}

fn check_tool(name: &str, custom_path: Option<&str>, version_args: &[&str], required: bool) -> ToolStatus {
    let candidate = custom_path.map(PathBuf::from).unwrap_or_else(|| PathBuf::from(name));

    match Command::new(&candidate).args(version_args).output() {
        Ok(output) => {
            let ver = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "available".to_string());

            let found_path = which::which(&candidate)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| candidate.display().to_string());

            ToolStatus {
                name: name.to_string(),
                status: "ok".to_string(),
                path: Some(found_path),
                version: Some(ver),
                required,
            }
        }
        Err(_) => {
            // Check if fallback name exists (e.g. "packager" for "shaka-packager")
            if name == "shaka-packager" {
                if let Ok(output) = Command::new("packager").args(version_args).output() {
                    let ver = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .next()
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|| "available".to_string());
                    return ToolStatus {
                        name: name.to_string(),
                        status: "ok".to_string(),
                        path: Some("packager".to_string()),
                        version: Some(ver),
                        required,
                    };
                }
            }

            ToolStatus {
                name: name.to_string(),
                status: "not_found".to_string(),
                path: None,
                version: None,
                required,
            }
        }
    }
}

mod which {
    use std::env;
    use std::path::{Path, PathBuf};

    pub fn which(name: &Path) -> Result<PathBuf, ()> {
        if name.is_absolute() || name.parent().map(|p| !p.as_os_str().is_empty()).unwrap_or(false) {
            if name.exists() {
                return Ok(name.to_path_buf());
            }
            return Err(());
        }

        if let Ok(path_var) = env::var("PATH") {
            for dir in env::split_paths(&path_var) {
                let candidate = dir.join(name);
                if candidate.exists() {
                    return Ok(candidate);
                }
                #[cfg(windows)]
                {
                    let candidate_exe = dir.join(format!("{}.exe", name.display()));
                    if candidate_exe.exists() {
                        return Ok(candidate_exe);
                    }
                }
            }
        }
        Err(())
    }
}
