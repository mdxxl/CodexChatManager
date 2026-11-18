#![windows_subsystem = "windows"]

mod catalog;
mod discovery;
mod drawing;
mod language;
mod rpc;
mod service;
mod time;
mod ui;

use language::{tr, Language};
use serde_json::json;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR};

const VERSION: &str = env!("CARGO_PKG_VERSION");
type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

fn err(message: impl Into<String>) -> Error {
    io::Error::other(message.into()).into()
}

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

fn main() {
    Language::saved().set_current();
    let args: HashSet<_> = std::env::args().skip(1).collect();
    let smoke = args.contains("--smoke-test");
    let locate = args.contains("--locate");
    let check = args.contains("--check");
    let help = args.contains("--help") || args.contains("-h");
    let demo = args.contains("--demo");
    let invalid = args.iter().any(|arg| {
        !matches!(
            arg.as_str(),
            "--demo" | "--check" | "--locate" | "--smoke-test" | "--help" | "-h"
        )
    });
    let console = smoke || locate || check || help || invalid;
    if console {
        // Inherited redirected pipes are already usable. Attach to an existing
        // console only when launched interactively with command-line options.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
    let result = (|| {
        if invalid || smoke && !demo {
            return Err(err(if invalid {
                tr(
                    "Неизвестный аргумент. Используйте --help.",
                    "Unknown argument. Use --help.",
                )
            } else {
                tr(
                    "--smoke-test требует --demo",
                    "--smoke-test requires --demo",
                )
            }));
        }
        if help {
            stdout(tr(
                "Менеджер локальных чатов Codex\n\n  --demo    Вымышленные чаты\n  --locate  Найти Codex CLI\n  --check   Проверить подключение",
                "Local Codex chat manager\n\n  --demo    Sample chats\n  --locate  Find Codex CLI\n  --check   Check the connection",
            ))?;
        } else if locate {
            let executable = discovery::find_codex(None)?;
            stdout(
                &json!({"executable": executable, "version": discovery::cli_version(&executable)})
                    .to_string(),
            )?;
        } else if check {
            let mut service = service::Service::new(false);
            let rows = service.list_all()?;
            stdout(
                &json!({"ok":true, "total":rows.len(),
                "archived":rows.iter().filter(|row| row.archived).count()})
                .to_string(),
            )?;
        } else {
            ui::run(demo, smoke)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if console {
            let _ = writeln!(io::stderr(), "{error}");
        } else {
            unsafe {
                MessageBoxW(
                    std::ptr::null_mut(),
                    wide(error.to_string()).as_ptr(),
                    wide("Codex Chat Manager").as_ptr(),
                    MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}

fn stdout(text: &str) -> Result<()> {
    writeln!(io::stdout(), "{text}")?;
    Ok(())
}
