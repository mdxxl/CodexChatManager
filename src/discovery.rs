use crate::language::{tr, trf};
use crate::{err, Result};
use glob::{glob_with, MatchOptions};
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub fn command(exe: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(exe);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn output(mut command: Command, timeout: u64) -> Result<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = sender.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
    });
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let result = (|| {
        let bytes = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))??;
        loop {
            if let Some(status) = child.try_wait()? {
                return if status.success() {
                    Ok(String::from_utf8_lossy(&bytes)
                        .trim_start_matches('\u{feff}')
                        .to_owned())
                } else {
                    Err(err(trf!(
                        "Процесс завершился с кодом {status}",
                        "Process exited with status {status}"
                    )))
                };
            }
            if Instant::now() >= deadline {
                return Err(err(tr(
                    "Истекло время ожидания процесса",
                    "The process timed out",
                )));
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn powershell(script: &str) -> Result<String> {
    let exe = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| r"C:\Windows".into())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut child = command(exe);
    child.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ]);
    output(child, 10)
}

pub fn require_closed_codex() -> Result<()> {
    require_closed_codex_with(
        tr(concat!(
            "Перед удалением завершите задачи и полностью закройте окно приложения Codex. ",
            "Codex Chat Manager оставьте открытым. Затем повторите удаление.\n\n",
            "Это необходимо, чтобы в боковой панели не оставались удалённые чаты ",
            "с ошибкой «no rollout found». После удаления откройте Codex заново."
        ), concat!(
            "Finish any running tasks and close Codex completely. Keep Codex Chat Manager open, then try again.\n\n",
            "Closing Codex prevents deleted chats from remaining in its sidebar with a “no rollout found” error. ",
            "Reopen Codex after deletion."
        )),
        tr("Не удалось проверить, закрыто ли окно Codex. Удаление не начато.",
            "Could not verify that Codex is closed. Deletion did not start."),
    )
}

pub fn require_closed_codex_for_cleanup() -> Result<()> {
    require_closed_codex_with(
        tr("Полностью закройте приложение Codex, затем повторите очистку старых ссылок. Откройте Codex снова после очистки.",
            "Close Codex completely, then try removing the stale links again. Reopen Codex afterward."),
        tr("Не удалось проверить, закрыто ли приложение Codex. Очистка ссылок остановлена.",
            "Could not verify that Codex is closed. Stale links were kept."),
    )
}

fn require_closed_codex_with(running: &str, unknown: &str) -> Result<()> {
    match powershell(include_str!("desktop_guard.ps1"))
        .as_deref()
        .map(str::trim)
    {
        Ok("closed") => Ok(()),
        Ok("running") => Err(err(running)),
        _ => Err(err(unknown)),
    }
}

pub fn cli_version(path: &Path) -> Option<String> {
    let mut child = command(path);
    child.arg("--version");
    let response = output(child, 6).ok()?;
    Regex::new(r"\bcodex-cli\s+([^\s]+)")
        .unwrap()
        .captures(&response)
        .map(|capture| capture[1].to_owned())
}

pub fn home() -> PathBuf {
    env::var_os("USERPROFILE")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(env::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn path_from_env(name: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(name)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}

fn recent_first(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let digits = Regex::new(r"[0-9]+").unwrap();
    paths.sort_by_cached_key(|path| {
        std::cmp::Reverse(
            digits
                .find_iter(&path.to_string_lossy())
                .map(|n| n.as_str().parse::<u64>().unwrap_or(u64::MAX))
                .collect::<Vec<_>>(),
        )
    });
    paths
}

fn add_glob(paths: &mut Vec<PathBuf>, root: &Path, pattern: &str) {
    let pattern = format!(
        "{}/{}",
        glob::Pattern::escape(&root.to_string_lossy()),
        pattern
    );
    let options = MatchOptions {
        case_sensitive: false,
        require_literal_separator: true,
        ..MatchOptions::new()
    };
    if let Ok(entries) = glob_with(&pattern, options) {
        paths.extend(recent_first(
            entries.filter_map(std::result::Result::ok).collect(),
        ));
    }
}

fn add_npm(paths: &mut Vec<PathBuf>, npm: &Path) {
    add_glob(paths, npm, "codex*/vendor/*/codex/codex.exe");
    add_glob(
        paths,
        &npm.join("codex"),
        "node_modules/@openai/codex*/vendor/*/codex/codex.exe",
    );
}

fn store_candidates() -> Vec<PathBuf> {
    let data = powershell(include_str!("codex_locations.ps1"))
        .ok()
        .and_then(|value| serde_json::from_str::<Value>(&value).ok())
        .unwrap_or_default();
    let mut paths = Vec::new();
    for base in strings(&data["installs"]).map(Path::new) {
        paths.extend([
            base.join("app/resources/codex.exe"),
            base.join("resources/codex.exe"),
        ]);
    }
    paths.extend(
        strings(&data["processes"])
            .map(PathBuf::from)
            .filter(|path| {
                path.parent().and_then(Path::file_name).is_some_and(|name| {
                    matches!(
                        name.to_string_lossy().to_ascii_lowercase().as_str(),
                        "resources" | "codex" | "bin" | "windows-x86_64" | "windows-aarch64"
                    )
                })
            }),
    );
    recent_first(paths)
}

fn strings(value: &Value) -> impl Iterator<Item = &str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn local_candidates() -> Vec<PathBuf> {
    let home = home();
    let local = path_from_env("LOCALAPPDATA", home.join("AppData/Local"));
    let roaming = path_from_env("APPDATA", home.join("AppData/Roaming"));
    let default_programs = PathBuf::from(r"C:\Program Files");
    let programs = path_from_env("ProgramFiles", default_programs.clone());
    let programs_64 = path_from_env("ProgramW6432", default_programs.clone());
    let mut paths = Vec::new();
    for base in [
        local.join("Programs/Codex"),
        local.join("Programs/OpenAI/Codex"),
        local.join("Programs/ChatGPT"),
        local.join("OpenAI/Codex"),
        programs.join("Codex"),
        programs_64.join("OpenAI/Codex"),
    ] {
        paths.extend([
            base.join("app/resources/codex.exe"),
            base.join("resources/codex.exe"),
        ]);
        add_glob(&mut paths, &base, "bin/*/codex.exe");
        add_glob(&mut paths, &base, "bin/*/resources/codex.exe");
    }
    for npm in [
        roaming.join("npm/node_modules/@openai"),
        local.join("npm/node_modules/@openai"),
        programs.join("nodejs/node_modules/@openai"),
    ] {
        add_npm(&mut paths, &npm);
    }
    for editor in [".vscode", ".vscode-insiders", ".cursor"] {
        add_glob(
            &mut paths,
            &home.join(editor).join("extensions"),
            "openai.chatgpt-*/bin/*/codex.exe",
        );
    }
    for root in [programs_64, programs, default_programs] {
        for package in ["OpenAI.Codex_*", "OpenAI.ChatGPT*"] {
            add_glob(
                &mut paths,
                &root.join("WindowsApps"),
                &format!("{package}/app/resources/codex.exe"),
            );
        }
    }
    paths.extend([
        home.join(".cargo/bin/codex.exe"),
        home.join("scoop/apps/codex/current/codex.exe"),
    ]);
    add_glob(
        &mut paths,
        &home.join("scoop/apps/codex/current"),
        "*/codex/codex.exe",
    );
    paths
}

fn path_candidates(override_path: Option<PathBuf>) -> Vec<PathBuf> {
    let configured = env::var_os("CODEX_MANAGER_EXECUTABLE").map(|value| {
        let trimmed = value.to_string_lossy();
        let trimmed = trimmed.trim().trim_matches('"');
        trimmed
            .strip_prefix("~/")
            .or_else(|| trimmed.strip_prefix("~\\"))
            .map_or_else(|| PathBuf::from(trimmed), |suffix| home().join(suffix))
    });
    let mut paths: Vec<_> = override_path.or(configured).into_iter().collect();
    let environment = env::var_os("PATH").unwrap_or_default();
    let extensions =
        env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    for name in ["codex.exe", "codex"] {
        for directory in env::split_paths(&environment).filter(|p| !p.as_os_str().is_empty()) {
            let mut names = vec![name.to_owned()];
            if name == "codex" {
                names.extend(
                    extensions
                        .to_string_lossy()
                        .split(';')
                        .map(|ext| format!("{name}{ext}")),
                );
            }
            if let Some(candidate) = names
                .iter()
                .map(|name| directory.join(name))
                .find(|p| p.is_file())
            {
                if !matches!(
                    candidate
                        .extension()
                        .and_then(OsStr::to_str)
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .as_str(),
                    "cmd" | "bat" | "ps1"
                ) {
                    paths.push(candidate);
                }
                add_npm(&mut paths, &directory.join("node_modules/@openai"));
                break;
            }
        }
    }
    paths
}

fn display_path(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(path) = text.strip_prefix(r"\\?\") {
        PathBuf::from(path)
    } else {
        path
    }
}

pub fn find_codex(override_path: Option<PathBuf>) -> Result<PathBuf> {
    let mut seen = HashSet::new();
    let mut try_paths = |paths: Vec<PathBuf>| {
        paths.into_iter().find_map(|path| {
            if !path.is_file() {
                return None;
            }
            let path = display_path(path.canonicalize().unwrap_or(path));
            if !seen.insert(path.to_string_lossy().to_lowercase()) {
                return None;
            }
            cli_version(&path).map(|_| path)
        })
    };
    try_paths(path_candidates(override_path)).or_else(|| try_paths(store_candidates()))
        .or_else(|| try_paths(local_candidates())).ok_or_else(|| err(tr(concat!(
            "Codex CLI не найден автоматически. Проверены установленные приложения Windows, PATH, ",
            "обычные папки установки, npm и расширения VS Code/Cursor. ",
            "Откройте установленный Codex и нажмите «Найти Codex» либо укажите консольный codex.exe вручную."
        ), concat!(
            "Codex CLI was not found in installed Windows apps, PATH, common installation folders, npm, or VS Code/Cursor extensions. ",
            "Open Codex and select Find Codex, or choose the Codex CLI executable manually."
        ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_versions_sort_numerically() {
        let newer: PathBuf = "OpenAI.Codex_26.1103.0/app/resources/codex.exe".into();
        let older = "OpenAI.Codex_26.999.0/app/resources/codex.exe".into();
        assert_eq!(recent_first(vec![older, newer.clone()])[0], newer);
    }
}
