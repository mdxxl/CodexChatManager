use crate::discovery::command;
use crate::language::{tr, trf};
use crate::{err, Result, VERSION};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

struct Connection {
    child: Child,
    input: Option<ChildStdin>,
    messages: Receiver<Value>,
}

pub trait Rpc {
    fn start(&mut self) -> Result<()>;
    fn call(&mut self, method: &str, params: Value, timeout: u64) -> Result<Value>;
    fn connected(&self) -> bool;
}

pub struct RpcClient {
    pub executable: PathBuf,
    #[cfg(test)]
    pub fixture: Option<(PathBuf, Vec<String>)>,
    connection: Option<Connection>,
    next_id: u64,
}

impl RpcClient {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            connection: None,
            next_id: 0,
            #[cfg(test)]
            fixture: None,
        }
    }

    fn command(&self) -> Command {
        #[cfg(test)]
        if let Some((path, args)) = &self.fixture {
            let mut process = command(path);
            process.args(args);
            return process;
        }
        let mut process = command(&self.executable);
        process.args(["app-server", "--stdio"]);
        process
    }

    fn write(&mut self, message: Value) -> Result<()> {
        let connection = self.connection.as_mut().ok_or_else(closed)?;
        if connection.child.try_wait()?.is_some() {
            self.close();
            return Err(closed());
        }
        let input = connection.input.as_mut().ok_or_else(closed)?;
        let result = serde_json::to_writer(&mut *input, &message).is_ok()
            && input.write_all(b"\n").and_then(|_| input.flush()).is_ok();
        if !result {
            self.close();
            return Err(err(tr(
                "Не удалось отправить команду локальному серверу Codex.",
                "Could not send a command to the local Codex server.",
            )));
        }
        Ok(())
    }

    pub fn close(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            connection.input.take();
            let deadline = Instant::now() + Duration::from_secs(2);
            while matches!(connection.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if !matches!(connection.child.try_wait(), Ok(Some(_))) {
                let _ = connection.child.kill();
                let _ = connection.child.wait();
            }
        }
    }
}

fn closed() -> crate::Error {
    err(tr(
        "Соединение с Codex закрыто. Нажмите «Обновить».",
        "The Codex connection is closed. Refresh the chat list.",
    ))
}

impl Rpc for RpcClient {
    fn start(&mut self) -> Result<()> {
        if let Some(connection) = &mut self.connection {
            if matches!(connection.child.try_wait(), Ok(None)) {
                return Ok(());
            }
            self.close();
        }
        let mut child = self
            .command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|exc| {
                err(trf!(
                    "Не удалось запустить Codex: {exc}",
                    "Could not start Codex: {exc}"
                ))
            })?;
        let (input, stdout) = (
            child.stdin.take(),
            child.stdout.take().expect("piped stdout"),
        );
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n') {
                let Ok(line) = line else { break };
                if let Ok(message @ Value::Object(_)) =
                    serde_json::from_str(&String::from_utf8_lossy(&line))
                {
                    if sender.send(message).is_err() {
                        break;
                    }
                }
            }
        });
        self.connection = Some(Connection {
            child,
            input,
            messages,
        });
        let result = self.call("initialize", json!({
            "clientInfo": {"name":"codex_chat_manager", "title":"Codex Chat Manager", "version":VERSION}
        }), 25).and_then(|_| self.write(json!({"method":"initialized"})));
        if result.is_err() {
            self.close();
        }
        result
    }

    fn call(&mut self, method: &str, params: Value, timeout: u64) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({"id":id, "method":method, "params":params}))?;
        let deadline = Instant::now() + Duration::from_secs(timeout);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.close();
                return Err(err(trf!(
                    "Codex не ответил на {method}. Обновите список перед повтором.",
                    "Codex did not respond to {method}. Refresh the list before trying again."
                )));
            }
            let next = self
                .connection
                .as_ref()
                .ok_or_else(closed)?
                .messages
                .recv_timeout(remaining);
            let message = match next {
                Ok(value) => value,
                Err(error) => {
                    self.close();
                    return Err(err(match error {
                        RecvTimeoutError::Timeout => {
                            trf!("Истекло время ожидания {method}. Обновите список.", "{method} timed out. Refresh the chat list.")
                        }
                        RecvTimeoutError::Disconnected => {
                            tr("Codex завершил соединение. Проверьте путь к CLI и доступ к профилю.",
                                "Codex closed the connection. Check the CLI path and access to the profile.").to_owned()
                        }
                    }));
                }
            };
            if message.get("method").is_some() {
                if let Some(id) = message.get("id") {
                    self.write(json!({"id":id, "error":{"code":-32601,
                        "message":"This client only manages chat history"}}))?;
                }
                continue;
            }
            if message["id"].as_u64() == Some(id) {
                if let Some(error) = message.get("error") {
                    if error["code"].as_i64() == Some(-32601) {
                        return Err(err(trf!(
                            "Эта версия Codex не поддерживает {method}. Обновите Codex.",
                            "This version of Codex does not support {method}. Update Codex."
                        )));
                    }
                    return Err(err(trf!(
                        "{method}: {} (код {})",
                        "{method}: {} (code {})",
                        error["message"]
                            .as_str()
                            .unwrap_or_else(|| tr("ошибка Codex", "Codex error")),
                        error["code"]
                    )));
                }
                return Ok(message.get("result").cloned().unwrap_or_else(|| json!({})));
            }
        }
    }

    fn connected(&self) -> bool {
        self.connection.is_some()
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.close();
    }
}
