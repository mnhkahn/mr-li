//! Persistent, bounded NDJSON JSON-RPC transport. No shell or terminal emulation.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex as AsyncMutex, broadcast, oneshot},
};

const MAX_FRAME: usize = 8 * 1024 * 1024;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;
pub struct Rpc {
    input: AsyncMutex<ChildStdin>,
    child: AsyncMutex<Child>,
    pending: Pending,
    events: broadcast::Sender<Value>,
    next: AtomicU64,
    reader: tokio::task::JoinHandle<()>,
    stderr: tokio::task::JoinHandle<()>,
}
impl Drop for Rpc {
    fn drop(&mut self) {
        if let Ok(child) = self.child.try_lock()
            && let Some(pid) = child.id()
        {
            // Each managed runtime owns a separate process group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        self.reader.abort();
        self.stderr.abort();
    }
}
impl Rpc {
    pub fn spawn(command: &mut Command) -> Result<Arc<Self>> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .context("无法启动 Agent CLI；请安装并在部署用户下完成登录")?;
        let input = child.stdin.take().context("Agent stdin 不可用")?;
        let output = child.stdout.take().context("Agent stdout 不可用")?;
        let mut errors = child.stderr.take().context("Agent stderr 不可用")?;
        // Drain without logging potentially sensitive provider/auth output.
        let stderr = tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while matches!(errors.read(&mut b).await, Ok(n) if n > 0) {}
        });
        let pending: Pending = Default::default();
        let waiting = pending.clone();
        let (events, _) = broadcast::channel(1024);
        let notify = events.clone();
        let reader = tokio::spawn(async move {
            let mut output = BufReader::new(output);
            loop {
                let frame = read_frame(&mut output).await;
                let Ok(Some(frame)) = frame else { break };
                let Ok(value) = serde_json::from_slice::<Value>(&frame) else {
                    break;
                };
                if value.get("method").is_none()
                    && let Some(id) = value["id"].as_u64()
                {
                    if let Some(tx) = waiting.lock().unwrap().remove(&id) {
                        let _ = tx.send(value);
                    }
                } else {
                    let _ = notify.send(value);
                }
            }
            waiting.lock().unwrap().clear();
            let _ = notify.send(json!({"method":"mr-li/disconnected"}));
        });
        Ok(Arc::new(Self {
            input: AsyncMutex::new(input),
            child: AsyncMutex::new(child),
            pending,
            events,
            next: AtomicU64::new(1),
            reader,
            stderr,
        }))
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }
    pub async fn alive(&self) -> bool {
        matches!(self.child.lock().await.try_wait(), Ok(None)) && !self.reader.is_finished()
    }
    pub async fn send(&self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        ensure!(bytes.len() < MAX_FRAME, "Agent 请求过大");
        bytes.push(b'\n');
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut input = self.input.lock().await;
            input.write_all(&bytes).await?;
            input.flush().await
        })
        .await
        .context("Agent 写入超时")??;
        Ok(())
    }
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let result = async {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await?;
            let response = tokio::time::timeout(timeout, rx)
                .await
                .context("Agent 请求超时")?
                .context("Agent 连接已断开")?;
            if let Some(error) = response.get("error") {
                // Preserve actionable auth/quota messages, but bound untrusted output.
                let message = error["message"].as_str().unwrap_or("请求失败");
                bail!(
                    "Agent {}：{}",
                    error["code"],
                    message.chars().take(600).collect::<String>()
                );
            }
            response
                .get("result")
                .cloned()
                .context("Agent 响应缺少 result")
        }
        .await;
        self.pending.lock().unwrap().remove(&id);
        result
    }
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }
    pub async fn reply(&self, id: Value, result: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await
    }
    pub async fn unsupported(&self, id: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client capability unavailable"}})).await
    }
    pub async fn kill(&self) -> Result<()> {
        let mut child = self.child.lock().await;
        if child.try_wait()?.is_none()
            && let Some(pid) = child.id()
        {
            let status = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            if status != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        child.wait().await?;
        Ok(())
    }
}
async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            ensure!(frame.is_empty(), "Agent JSON 帧被截断");
            return Ok(None);
        }
        let length = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        ensure!(frame.len() + length <= MAX_FRAME, "Agent JSON 帧过大");
        frame.extend_from_slice(&bytes[..length]);
        reader.consume(length);
        if frame.last() == Some(&b'\n') {
            return Ok(Some(frame));
        }
    }
}
/// The worker owns the CLI's process group. Parent pipe EOF (including a bridge
/// crash) terminates the whole group, so a restarted bridge cannot orphan tools.
pub async fn worker() -> Result<()> {
    ensure!(
        std::env::args().nth(2).as_deref() == Some("--"),
        "内部 Agent worker 参数无效"
    );
    let args: Vec<_> = std::env::args_os().skip(3).collect();
    let program = args.first().context("内部 Agent worker 缺少程序")?;
    ensure!(
        unsafe { libc::getpgrp() == libc::getpid() || libc::setpgid(0, 0) == 0 },
        "无法隔离 Agent 进程组"
    );
    let mut child = Command::new(program)
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().context("stdin")?;
    let mut output = child.stdout.take().context("stdout")?;
    let mut errors = child.stderr.take().context("stderr")?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    tokio::select! {
        _=tokio::io::copy(&mut stdin,&mut input)=>{},
        _=tokio::io::copy(&mut output,&mut stdout)=>{},
        _=tokio::io::copy(&mut errors,&mut stderr)=>{},
        _=child.wait()=>{},
    }
    // The worker is also killed here. This ensures grandchildren cannot outlive
    // a lost transport, even if the CLI ignores EOF or cancellation.
    unsafe {
        libc::kill(-libc::getpgrp(), libc::SIGKILL);
    }
    bail!("Agent worker 无法停止进程组")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn framing_rejects_truncation_and_oversize() {
        assert!(read_frame(&mut &b"{\"id\":1}"[..]).await.is_err());
        let bytes = vec![b'x'; MAX_FRAME + 1];
        assert!(read_frame(&mut &bytes[..]).await.is_err());
        assert_eq!(
            read_frame(&mut &b"{}\n"[..]).await.unwrap(),
            Some(b"{}\n".to_vec())
        );
    }
}
