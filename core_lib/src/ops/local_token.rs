//! 本机回环 HTTP 共享令牌。
//!
//! sidecar 场景下，服务只监听 127.0.0.1；本模块再加一道共享令牌，
//! 防止本机其他进程（或浏览器中的恶意页面）直接调用 /backend 的 ops 写接口。
//! 令牌每个进程启动时重新生成，写入 0600 的 session.token；可用环境变量覆盖。

use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct LocalToken {
    token: String,
    path: Option<PathBuf>,
}

pub const HEADER: &str = "x-ops-token";

impl LocalToken {
    pub fn init(dir: &Path) -> Self {
        let token = std::env::var("LOCAL_OPS_TOKEN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                (uuid::Uuid::new_v4().to_string() + &uuid::Uuid::new_v4().to_string())
                    .replace('-', "")
            });
        let path = dir.join("session.token");
        let mut instance = Self {
            token,
            path: Some(path),
        };
        let _ = instance.persist();
        instance
    }

    pub fn for_test(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            path: None,
        }
    }

    fn persist(&mut self) -> Result<(), std::io::Error> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(self.token.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// 恒定时间比较，避免简单的时序侧信道。
    pub fn matches(&self, candidate: Option<&str>) -> bool {
        let Some(candidate) = candidate else {
            return false;
        };
        let expected = self.token.as_bytes();
        let actual = candidate.trim().as_bytes();
        if expected.len() != actual.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(actual.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_exact() {
        let token = LocalToken::for_test("abc123");
        assert!(token.matches(Some("abc123")));
        assert!(token.matches(Some(" abc123 ")));
        assert!(!token.matches(Some("abc124")));
        assert!(!token.matches(Some("abc")));
        assert!(!token.matches(None));
    }
}
