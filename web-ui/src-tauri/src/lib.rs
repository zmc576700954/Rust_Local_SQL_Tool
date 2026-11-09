use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager, RunEvent, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;

const SIDECAR_BIN_NAME: &str = "web-server";
const SIDECAR_READY_PREFIX: &str = "SIDECAR_READY ";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const LOG_TAIL_LINES: usize = 40;

#[derive(Clone)]
enum BackendStatus {
    Starting,
    /// Base URL of the local backend, e.g. `http://127.0.0.1:12345/backend`.
    Ready(String),
    Failed(String),
}

struct BackendInner {
    status: Mutex<BackendStatus>,
    child: Mutex<Option<Child>>,
    log_path: PathBuf,
    log_file: Mutex<Option<std::fs::File>>,
    log_tail: Mutex<Vec<String>>,
    shutting_down: AtomicBool,
    failure_shown: AtomicBool,
}

#[derive(Clone)]
struct BackendRuntime {
    inner: Arc<BackendInner>,
}

impl BackendRuntime {
    fn new(log_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(BackendInner {
                status: Mutex::new(BackendStatus::Starting),
                child: Mutex::new(None),
                log_path,
                log_file: Mutex::new(None),
                log_tail: Mutex::new(Vec::new()),
                shutting_down: AtomicBool::new(false),
                failure_shown: AtomicBool::new(false),
            }),
        }
    }

    fn status(&self) -> BackendStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::SeqCst)
    }

    fn mark_ready(&self, base_url: String) {
        let mut status = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if matches!(*status, BackendStatus::Starting) {
            *status = BackendStatus::Ready(base_url);
        }
    }

    fn write_log(&self, stream: &str, line: &str) {
        // 写前清洗：sidecar 输出的驱动/网络错误可能带 DSN 或 host:port。
        let (clean, _) = core_lib::ops::inspection::redact::scrub_text(line, &[]);
        let entry = format!(
            "{} [{stream}] {clean}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
        );
        if let Ok(mut guard) = self.inner.log_file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = writeln!(file, "{entry}");
                let _ = file.flush();
            }
        }
        if let Ok(mut tail) = self.inner.log_tail.lock() {
            tail.push(entry);
            if tail.len() > LOG_TAIL_LINES {
                tail.remove(0);
            }
        }
    }

    fn try_wait_child(&self) -> Option<std::process::ExitStatus> {
        let mut guard = self
            .inner
            .child
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        match guard.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status),
                _ => None,
            },
            None => None,
        }
    }

    fn fail(&self, app: &AppHandle, reason: impl Into<String>) {
        if self.is_shutting_down() {
            return;
        }
        let reason = reason.into();
        {
            let mut status = self
                .inner
                .status
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if matches!(*status, BackendStatus::Failed(_)) {
                return;
            }
            *status = BackendStatus::Failed(reason.clone());
        }

        if self.inner.failure_shown.swap(true, Ordering::SeqCst) {
            return;
        }

        let tail = self
            .inner
            .log_tail
            .lock()
            .map(|tail| tail.join("\n"))
            .unwrap_or_default();
        let message = format!(
            "{reason}\n\n日志文件：{}\n\n最近日志：\n{}",
            self.inner.log_path.display(),
            if tail.is_empty() { "(无)" } else { &tail }
        );
        let _ = app
            .dialog()
            .message(message)
            .title("本地后端服务不可用")
            .blocking_show();
    }
}

fn parse_ready_base_url(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(SIDECAR_READY_PREFIX)?;
    let url = rest.trim().trim_end_matches('/');
    if !url.starts_with("http://127.0.0.1:") && !url.starts_with("http://localhost:") {
        return None;
    }
    Some(format!("{url}/backend"))
}

fn sidecar_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| format!("无法定位主程序路径: {err}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "无法定位主程序目录".to_string())?;
    let candidate = dir.join(format!("{SIDECAR_BIN_NAME}{}", std::env::consts::EXE_SUFFIX));
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "未找到 sidecar 可执行文件：{}。请先运行 npm run sidecar:build，或使用 npm run tauri:dev / npm run tauri:build 自动构建。",
            candidate.display()
        ))
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    app: AppHandle,
    runtime: BackendRuntime,
    reader: R,
    is_stdout: bool,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let text = line.trim_end_matches(['\r', '\n']);
                    runtime.write_log(if is_stdout { "out" } else { "err" }, text);
                    if is_stdout {
                        if let Some(base_url) = parse_ready_base_url(text) {
                            runtime.mark_ready(base_url);
                        }
                    }
                }
                Err(_) => break,
            }
        }

        if is_stdout && !runtime.is_shutting_down() {
            // The sidecar exits on its own after stdout closes; give it a moment
            // so we can report the real exit status instead of a generic message.
            thread::sleep(Duration::from_millis(150));
            if !runtime.is_shutting_down() {
                let reason = match runtime.try_wait_child() {
                    Some(status) => format!("本地后端进程已退出（{status}）"),
                    None => "本地后端进程的输出流已关闭".to_string(),
                };
                runtime.fail(&app, reason);
            }
        }
    });
}

fn spawn_watchdog(app: AppHandle, runtime: BackendRuntime) {
    thread::spawn(move || {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if runtime.is_shutting_down() {
                return;
            }
            if !matches!(runtime.status(), BackendStatus::Starting) {
                return;
            }
            if let Some(status) = runtime.try_wait_child() {
                runtime.fail(&app, format!("本地后端进程在就绪前退出（{status}）"));
                return;
            }
            if Instant::now() >= deadline {
                runtime.fail(
                    &app,
                    format!("本地后端服务在 {} 秒内未就绪", READY_TIMEOUT.as_secs()),
                );
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

fn open_log_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn start_sidecar(app: &AppHandle, runtime: &BackendRuntime) {
    let path = match sidecar_path() {
        Ok(path) => path,
        Err(reason) => {
            runtime.fail(app, reason);
            return;
        }
    };

    if let Some(parent) = runtime.inner.log_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            runtime.fail(
                app,
                format!(
                    "无法创建 sidecar 日志目录 {}: {err}",
                    parent.display()
                ),
            );
            return;
        }
    }
    match open_log_file(&runtime.inner.log_path) {
        Ok(file) => {
            if let Ok(mut guard) = runtime.inner.log_file.lock() {
                *guard = Some(file);
            }
        }
        Err(err) => {
            runtime.fail(
                app,
                format!(
                    "无法创建 sidecar 日志文件 {}: {err}",
                    runtime.inner.log_path.display()
                ),
            );
            return;
        }
    }

    // Port 0 lets the OS pick a free port, so two app instances never fight over
    // a fixed port. The sidecar announces the actual address on stdout.
    let spawned = Command::new(&path)
        .env("BIND_ADDR", "127.0.0.1:0")
        .env("SIDECAR_READY_STDOUT", "1")
        .env("EXIT_ON_STDIN_EOF", "1")
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            runtime.fail(
                app,
                format!("无法启动 sidecar {}: {err}", path.display()),
            );
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    if let Ok(mut guard) = runtime.inner.child.lock() {
        *guard = Some(child);
    }

    if let Some(stdout) = stdout {
        spawn_reader(app.clone(), runtime.clone(), stdout, true);
    }
    if let Some(stderr) = stderr {
        spawn_reader(app.clone(), runtime.clone(), stderr, false);
    }
    spawn_watchdog(app.clone(), runtime.clone());
}

fn shutdown_sidecar(runtime: &BackendRuntime) {
    if runtime.inner.shutting_down.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut guard = runtime
        .inner
        .child
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let Some(mut child) = guard.take() else {
        return;
    };

    // Closing stdin is the graceful signal (web-server watches stdin EOF), then
    // force kill if it does not exit within the grace period.
    drop(child.stdin.take());
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[tauri::command]
async fn desktop_backend_base_url(runtime: State<'_, BackendRuntime>) -> Result<String, String> {
    let runtime = runtime.inner().clone();
    let deadline = Instant::now() + READY_TIMEOUT + Duration::from_secs(10);
    loop {
        match runtime.status() {
            BackendStatus::Ready(base_url) => return Ok(base_url),
            BackendStatus::Failed(reason) => return Err(reason),
            BackendStatus::Starting => {}
        }
        if Instant::now() >= deadline {
            return Err("本地后端服务仍未就绪".to_string());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Single-instance must be registered before other plugins.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![desktop_backend_base_url])
        .setup(|app| {
            let log_path = app
                .path()
                .app_log_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join("web-server.log");
            let runtime = BackendRuntime::new(log_path);
            app.manage(runtime.clone());
            start_sidecar(app.handle(), &runtime);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            RunEvent::WindowEvent {
                event: WindowEvent::CloseRequested { .. },
                ..
            } => {
                // Single-window utility: closing the window exits the app so the
                // sidecar never outlives a windowless process on macOS.
                app.exit(0);
            }
            RunEvent::ExitRequested { .. } | RunEvent::Exit => {
                if let Some(runtime) = app.try_state::<BackendRuntime>() {
                    shutdown_sidecar(&runtime);
                }
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::parse_ready_base_url;

    #[test]
    fn parses_sidecar_ready_line() {
        assert_eq!(
            parse_ready_base_url("SIDECAR_READY http://127.0.0.1:53267"),
            Some("http://127.0.0.1:53267/backend".to_string())
        );
        assert_eq!(
            parse_ready_base_url("  SIDECAR_READY http://127.0.0.1:53267/  "),
            Some("http://127.0.0.1:53267/backend".to_string())
        );
    }

    #[test]
    fn ignores_unrelated_or_unsafe_lines() {
        assert_eq!(parse_ready_base_url("INFO Server listening on http://127.0.0.1:3000"), None);
        assert_eq!(parse_ready_base_url("SIDECAR_READY http://0.0.0.0:3000"), None);
        assert_eq!(parse_ready_base_url("SIDECAR_READY"), None);
    }

    #[test]
    fn sidecar_log_is_private_and_scrubbed() {
        use super::{open_log_file, BackendRuntime};

        let path = std::env::temp_dir().join(format!("sidecar-log-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        drop(open_log_file(&path).unwrap());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "log file must be owner-only");
        }

        let runtime = BackendRuntime::new(path.clone());
        *runtime.inner.log_file.lock().unwrap() = Some(open_log_file(&path).unwrap());
        runtime.write_log(
            "stderr",
            "Failed to connect: mysql://root:rootpass@127.0.0.1:3306/shop?password=plain123",
        );
        runtime.write_log("stderr", "导入失败: postgres://ops:secret@10.0.0.5:5432/app");
        // 驱动认证失败：错误码、SQLSTATE、原因、账号、主机必须保留，只删除凭据本体。
        runtime.write_log(
            "stderr",
            "Failed to connect to database: error returned from database: (code: 1045) (SQLState: 28000) Access denied for user 'shop'@'localhost' (using password: YES) token:abc456 secret_access_key=AKIAEXAMPLE",
        );
        drop(runtime);

        let content = std::fs::read_to_string(&path).unwrap();
        println!("SIDECAR_LOG_BEGIN\n{content}\nSIDECAR_LOG_END");
        assert!(
            !core_lib::ops::inspection::redact::contains_sensitive_shape(&content),
            "log still sensitive: {content}"
        );
        for shape in [
            "rootpass",
            "plain123",
            "secret@",
            "password=",
            "abc456",
            "token:",
            "AKIAEXAMPLE",
        ] {
            assert!(!content.contains(shape), "log leaked {shape}: {content}");
        }
        // 主机、端口、库名保留，便于排查。
        assert!(content.contains("mysql://root@127.0.0.1:3306/shop"), "{content}");
        assert!(content.contains("postgres://ops@10.0.0.5:5432/app"), "{content}");
        assert!(content.contains("Failed to connect"));
        assert!(content.contains("导入失败"));
        // 认证失败诊断信息必须可读。
        for expected in [
            "1045",
            "28000",
            "SQLState",
            "Access denied",
            "'shop'@'localhost'",
            "localhost",
            "using password: YES",
        ] {
            assert!(content.contains(expected), "missing {expected}: {content}");
        }
        let _ = std::fs::remove_file(&path);
    }
}
