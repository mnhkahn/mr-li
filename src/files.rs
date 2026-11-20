//! Outbound files from the Agent workspace, uploaded from a bounded snapshot.
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const MAX_SIZE: u64 = 30 * 1024 * 1024;
const PREFIX: &str = "/_send-file ";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}
pub fn is_send(text: &str) -> bool {
    text.starts_with(PREFIX)
}
pub fn command(path: &str, file_name: Option<&str>) -> Result<String> {
    ensure!(
        !path.trim().is_empty() && path.len() <= 4096 && !path.contains('\0'),
        "文件路径无效"
    );
    if let Some(name) = file_name {
        validate_name(name)?;
    }
    Ok(format!(
        "{PREFIX}{}",
        serde_json::to_string(&Request {
            path: path.into(),
            file_name: file_name.map(str::to_owned)
        })?
    ))
}
pub fn parse(text: &str) -> Result<Request> {
    Ok(serde_json::from_str(
        text.strip_prefix(PREFIX).context("文件操作无效")?,
    )?)
}
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name != "."
            && name != ".."
            && name.chars().count() <= 200
            && !name
                .chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\')),
        "文件名无效，请使用不含路径或控制字符的文件名（最多200字）"
    );
    Ok(())
}
pub struct Prepared {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
pub async fn prepare(cfg: &Config, request: &Request) -> Result<Prepared> {
    ensure!(
        !request.path.trim().is_empty()
            && request.path.len() <= 4096
            && !request.path.contains('\0'),
        "文件路径无效"
    );
    let root = tokio::fs::canonicalize(&cfg.workspace)
        .await
        .context("工作目录不可用")?;
    let supplied = Path::new(&request.path);
    let path = tokio::fs::canonicalize(if supplied.is_absolute() {
        supplied.to_owned()
    } else {
        root.join(supplied)
    })
    .await
    .context("文件不存在或不可读取")?;
    ensure!(
        path.starts_with(&root),
        "只能发送 Agent 工作目录内的文件，请先将成果保存到工作目录"
    );
    let name = request
        .file_name
        .clone()
        .or_else(|| supplied.file_name()?.to_str().map(str::to_owned))
        .context("请指定有效的 file_name")?;
    validate_name(&name)?;
    // Do not follow a leaf swapped to a symlink, or block opening a FIFO.
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let source = tokio::fs::File::from_std(options.open(&path).context("无法读取文件")?);
    let metadata = source.metadata().await?;
    ensure!(metadata.is_file(), "只能发送普通文件，目录请先压缩");
    ensure!(
        metadata.len() > 0 && metadata.len() <= MAX_SIZE,
        "文件不能为空，且单文件不能超过30MB"
    );
    let directory = cfg.data_dir.join("outgoing-files");
    tokio::fs::create_dir_all(&directory).await?;
    let path = directory.join(format!("{}.bin", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut output = tokio::fs::File::from_std(options.open(&path)?);
    let mut prepared = Prepared {
        path,
        name,
        size: 0,
    };
    let size = tokio::io::copy(&mut source.take(MAX_SIZE + 1), &mut output).await?;
    ensure!(
        size > 0 && size <= MAX_SIZE,
        "文件不能为空，且单文件不能超过30MB"
    );
    output.flush().await?;
    prepared.size = size;
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> Result<(Fixture, Config)> {
        let dir = std::env::temp_dir().join(format!("mr-li-files-{}", uuid::Uuid::new_v4()));
        let mut cfg = Config::fixture();
        cfg.workspace = dir.join("workspace");
        cfg.data_dir = dir.join("data");
        std::fs::create_dir_all(&cfg.workspace)?;
        Ok((Fixture(dir), cfg))
    }
    #[tokio::test]
    async fn unicode_files_are_snapshotted_and_cleaned_without_changing_the_source() -> Result<()> {
        let (_f, cfg) = fixture()?;
        let source = cfg.workspace.join("报告 原稿.pdf");
        tokio::fs::write(&source, b"original").await?;
        let request = parse(&command("报告 原稿.pdf", Some("发给家长的报告.pdf"))?)?;
        let prepared = prepare(&cfg, &request).await?;
        assert_eq!(prepared.name, "发给家长的报告.pdf");
        assert_eq!(prepared.size, 8);
        tokio::fs::write(&source, b"changed").await?;
        assert_eq!(tokio::fs::read(&prepared.path).await?, b"original");
        let staged = prepared.path.clone();
        drop(prepared);
        assert!(!staged.exists());
        assert_eq!(tokio::fs::read(source).await?, b"changed");
        Ok(())
    }
    #[tokio::test]
    async fn rejects_escape_symlinks_directories_empty_and_oversized_files() -> Result<()> {
        let (f, cfg) = fixture()?;
        let outside = f.0.join("private.txt");
        std::fs::write(&outside, "secret")?;
        std::os::unix::fs::symlink(&outside, cfg.workspace.join("link.txt"))?;
        std::fs::write(cfg.workspace.join("empty"), b"")?;
        let big = std::fs::File::create(cfg.workspace.join("too-big"))?;
        big.set_len(MAX_SIZE + 1)?;
        for path in [
            "../private.txt",
            "link.txt",
            ".",
            "empty",
            "too-big",
            "missing",
        ] {
            let request = Request {
                path: path.into(),
                file_name: Some("result.txt".into()),
            };
            assert!(prepare(&cfg, &request).await.is_err(), "accepted {path}");
        }
        for name in ["../secret", "a/b", "a\\b", "a\n.txt", ".", "..", " "] {
            assert!(command("a", Some(name)).is_err());
        }
        assert!(command("\0", None).is_err());
        assert!(parse("/_send-file {\"path\":\"a\",\"chat_id\":\"other\"}").is_err());
        Ok(())
    }
}
