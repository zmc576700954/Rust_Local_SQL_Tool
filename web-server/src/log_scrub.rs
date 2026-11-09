//! 日志层清洗：在 `tracing` 事件写往 stderr/日志文件之前，删除凭据本体。
//!
//! 复用巡检的 `redact` 规则，覆盖数据库连接错误、导入导出错误与巡检错误等路径。
//! 驱动错误码、SQLSTATE、原因、账号、主机、端口与库名必须保留，便于排查；
//! 只删除密码、`user:pass@` 里的 pass、`password=`/`token=`/AK/SK 等凭据值。
//! 这是写前清洗，不依赖事后处理日志文件。

use std::fmt;

use core_lib::ops::inspection::redact::scrub_text;
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::{Format, FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::registry::LookupSpan;

pub struct ScrubbingFormat {
    inner: Format,
}

impl Default for ScrubbingFormat {
    fn default() -> Self {
        Self {
            inner: Format::default(),
        }
    }
}

impl<S, N> FormatEvent<S, N> for ScrubbingFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut rendered = String::new();
        self.inner
            .format_event(ctx, Writer::new(&mut rendered), event)?;
        let (clean, _) = scrub_text(&rendered, &[]);
        writeln!(writer, "{clean}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
        type Writer = SharedBuffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn record(execute: impl FnOnce()) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(ScrubbingFormat::default())
            .with_writer(buffer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, execute);
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    fn emit_sample_errors() {
        tracing::error!(
            "Failed to connect to database: mysql://root:rootpass@127.0.0.1:3306/shop?password=plain123"
        );
        tracing::warn!("导入任务失败: mysql://root:rootpass@127.0.0.1:3306/shop password=plain123");
        tracing::warn!("定期巡检执行失败: postgres://ops:secret@10.0.0.5:5432/app 连接超时");
    }

    fn assert_clean(log: &str) {
        assert!(
            !core_lib::ops::inspection::redact::contains_sensitive_shape(log),
            "log still sensitive: {log}"
        );
        for shape in [
            "rootpass",
            "plain123",
            "secret@",
            "password=",
        ] {
            assert!(!log.contains(shape), "log leaked {shape}: {log}");
        }
        assert!(log.contains("Failed to connect to database"));
        assert!(log.contains("导入任务失败"));
        assert!(log.contains("定期巡检执行失败"));
        // 主机、端口、库名与账号保留，便于定位问题。
        assert!(log.contains("mysql://root@127.0.0.1:3306/shop"), "{log}");
        assert!(log.contains("postgres://ops@10.0.0.5:5432/app"), "{log}");
    }

    #[test]
    fn tracing_events_are_scrubbed_before_output() {
        let output = record(emit_sample_errors);
        println!("LOG_CAPTURE_BEGIN\n{output}\nLOG_CAPTURE_END");
        assert_clean(&output);
    }

    /// 真库认证失败：错误码与原因必须保留，密码本体必须消失。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_auth_failure_keeps_error_code_and_reason() {
        let url = match std::env::var("IMPORT_SQL_MYSQL_URL") {
            Ok(url) if !url.trim().is_empty() && url.contains('@') => url,
            _ => return,
        };
        let bad = wrong_password_url(&url);
        let err = match core_lib::db::DbClient::new_default(&bad).await {
            Ok(_) => panic!("错误密码不应连接成功: {bad}"),
            Err(err) => err,
        };

        let output = record(|| tracing::error!("Failed to connect to database: {err}"));
        println!("AUTH_LOG_BEGIN\n{output}\nAUTH_LOG_END");
        for expected in ["1045", "Access denied", "using password: YES"] {
            assert!(output.contains(expected), "missing {expected}: {output}");
        }
        assert!(
            !output.contains("wrong-password"),
            "password leaked: {output}"
        );
        assert!(
            !core_lib::ops::inspection::redact::contains_sensitive_shape(&output),
            "log still sensitive: {output}"
        );
    }

    /// 把 URL 里的密码替换成固定错误值，保留账号、主机与库名。
    fn wrong_password_url(url: &str) -> String {
        match url.split_once("://") {
            Some((scheme, rest)) => match rest.split_once('@') {
                Some((userinfo, host)) => {
                    let user = userinfo.split(':').next().unwrap_or(userinfo);
                    format!("{scheme}://{user}:wrong-password@{host}")
                }
                None => url.to_string(),
            },
            None => url.to_string(),
        }
    }

    #[test]
    fn actual_log_file_has_no_sensitive_shapes() {
        let dir = std::env::temp_dir().join(format!("log-scrub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("backend.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let shared = Arc::new(Mutex::new(file));

        #[derive(Clone)]
        struct FileWriter(Arc<Mutex<std::fs::File>>);
        impl std::io::Write for FileWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.lock().unwrap().flush()
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileWriter {
            type Writer = FileWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let sink = FileWriter(shared);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(ScrubbingFormat::default())
            .with_writer(sink)
            .finish();
        tracing::subscriber::with_default(subscriber, emit_sample_errors);

        let content = std::fs::read_to_string(&path).unwrap();
        println!("LOG_FILE_BEGIN\n{content}\nLOG_FILE_END");
        assert_clean(&content);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
