use axum::{routing::post, Json, Router};
use chrono::{DateTime, Utc};
use core_lib::ai::gateway::AiGateway;
use core_lib::config::{AiConnectionMode, AiProvider, AppConfig};
use core_lib::db::DbClient;
use core_lib::mysql_sync::{MySqlDataSyncEngine, SyncMode};
use core_lib::perf_report::{PerformanceCase, PerformanceMetrics, PerformanceReport, PerformanceStage};
use serde_json::Value;
use sha2::Digest;
use sqlx::{mysql::MySqlPoolOptions, postgres::PgPoolOptions, sqlite::SqlitePoolOptions};
use sqlx::Row;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
fn push_case(
    cases: &mut Vec<PerformanceCase>,
    id: String,
    kind: &str,
    labels: HashMap<String, String>,
    duration_ms: u128,
    rows: Option<u64>,
    bytes: Option<u64>,
    stages: Vec<PerformanceStage>,
    extra: Option<Value>,
) {
    cases.push(PerformanceCase {
        id,
        kind: kind.to_string(),
        labels,
        metrics: PerformanceMetrics::new(duration_ms, rows, bytes),
        stages,
        extra,
    });
}

#[derive(Clone)]
struct RunConfig {
    mysql_url: String,
    mariadb_url: String,
    postgres_url: String,
    sqlite_url: String,
}

impl RunConfig {
    fn from_env() -> Self {
        let mysql_url = std::env::var("E2E_MYSQL_URL")
            .unwrap_or_else(|_| "mysql://root:password@127.0.0.1:3306/e2e".to_string());
        let mariadb_url = std::env::var("E2E_MARIADB_URL")
            .unwrap_or_else(|_| "mysql://root:password@127.0.0.1:3307/e2e".to_string());
        let postgres_url = std::env::var("E2E_POSTGRES_URL")
            .unwrap_or_else(|_| "postgres://postgres:password@127.0.0.1:5432/e2e".to_string());
        let sqlite_path = std::env::var("E2E_SQLITE_PATH").unwrap_or_else(|_| {
            let mut p = std::env::temp_dir();
            p.push("local-ai-sql-e2e.sqlite3");
            p.to_string_lossy().to_string()
        });
        let sqlite_url = if sqlite_path.starts_with("sqlite:") {
            sqlite_path
        } else {
            format!("sqlite://{}", sqlite_path)
        };
        Self {
            mysql_url,
            mariadb_url,
            postgres_url,
            sqlite_url,
        }
    }
}

#[derive(Clone, Debug)]
enum JobStatus {
    Pending,
    Running,
    Completed,
    Error(String),
}

#[derive(Clone, Debug)]
struct ExportArtifacts {
    data_path: PathBuf,
    #[allow(dead_code)]
    manifest_path: PathBuf,
    row_count: u64,
}

#[derive(Clone, Debug)]
struct ExportJob {
    status: JobStatus,
    artifacts: Option<ExportArtifacts>,
}

#[derive(Clone, Debug)]
struct ImportJob {
    status: JobStatus,
    inserted: Option<u64>,
}

#[derive(Clone, Default)]
struct JobStore {
    export_jobs: Arc<RwLock<HashMap<String, ExportJob>>>,
    import_jobs: Arc<RwLock<HashMap<String, ImportJob>>>,
}

#[derive(Clone)]
struct ExportSpec {
    name_like: String,
    pk_start: i64,
    pk_end: i64,
    window_limit: i64,
    window_offset: i64,
}

fn ensure(cond: bool, msg: impl Into<String>) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(msg.into())
    }
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

fn job_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push("local-ai-sql-e2e");
    p
}

async fn write_export_files(
    db_tag: &str,
    job_id: &str,
    rows: &[Value],
) -> Result<ExportArtifacts, String> {
    let dir = job_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;

    let data_path = dir.join(format!("export_{}_{}.json", db_tag, job_id));
    let manifest_path = dir.join(format!("export_{}_{}.manifest.json", db_tag, job_id));

    let data_bytes = serde_json::to_vec_pretty(rows).map_err(|e| e.to_string())?;
    tokio::fs::write(&data_path, &data_bytes)
        .await
        .map_err(|e| e.to_string())?;

    let mut hasher = sha2::Sha256::new();
    hasher.update(&data_bytes);
    let sha256 = format!("{:x}", hasher.finalize());

    let generated_at: DateTime<Utc> = Utc::now();
    let manifest = serde_json::json!({
        "schema_version": "1",
        "generated_at": generated_at.to_rfc3339(),
        "sha256": sha256,
        "bytes": data_bytes.len(),
        "row_count": rows.len(),
        "format": "json",
        "db": db_tag,
    });
    tokio::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?)
        .await
        .map_err(|e| e.to_string())?;

    Ok(ExportArtifacts {
        data_path,
        manifest_path,
        row_count: rows.len() as u64,
    })
}

async fn export_job_poll(store: &JobStore, job_id: &str) -> Result<ExportArtifacts, String> {
    for _ in 0..600 {
        let job = { store.export_jobs.read().await.get(job_id).cloned() };
        if let Some(j) = job {
            match j.status {
                JobStatus::Completed => return Ok(j.artifacts.unwrap()),
                JobStatus::Error(e) => return Err(e),
                JobStatus::Pending | JobStatus::Running => {}
            }
        }
        sleep_ms(50).await;
    }
    Err("export job timeout".to_string())
}

async fn import_job_poll(store: &JobStore, job_id: &str) -> Result<u64, String> {
    for _ in 0..600 {
        let job = { store.import_jobs.read().await.get(job_id).cloned() };
        if let Some(j) = job {
            match j.status {
                JobStatus::Completed => return Ok(j.inserted.unwrap_or(0)),
                JobStatus::Error(e) => return Err(e),
                JobStatus::Pending | JobStatus::Running => {}
            }
        }
        sleep_ms(50).await;
    }
    Err("import job timeout".to_string())
}

async fn start_mock_ai_server() -> Result<(SocketAddr, tokio::task::JoinHandle<()>), String> {
    async fn handler() -> Json<Value> {
        Json(serde_json::json!({
            "choices": [
                { "message": { "content": "pong" } }
            ]
        }))
    }

    let app = Router::new().route("/v1/chat/completions", post(handler));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((addr, handle))
}

async fn start_bad_http_proxy() -> Result<(SocketAddr, tokio::task::JoinHandle<()>), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let resp = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, resp).await;
            });
        }
    });
    Ok((addr, handle))
}

async fn ai_proxy_failure_split_e2e() -> Result<(), String> {
    let (mock_addr, mock_handle) = start_mock_ai_server().await?;
    let (proxy_addr, proxy_handle) = start_bad_http_proxy().await?;

    let prev_http_proxy = std::env::var("HTTP_PROXY").ok();
    let prev_https_proxy = std::env::var("HTTPS_PROXY").ok();

    let mut config = AppConfig::default();
    if let Some(p) = config.ai_profiles.first_mut() {
        p.provider = AiProvider::Openai;
        p.mode = AiConnectionMode::Relay;
        p.api_key = Some("test".to_string());
        p.relay_url = Some(format!("http://{}/v1/chat/completions", mock_addr));
    }

    std::env::remove_var("HTTP_PROXY");
    std::env::remove_var("HTTPS_PROXY");
    let ok = AiGateway::new(config.clone()).health_check().await;
    ensure(ok.is_ok(), format!("ai health should succeed: {:?}", ok.err()))?;

    std::env::set_var("HTTP_PROXY", format!("http://{}", proxy_addr));
    let fail = AiGateway::new(config).health_check().await;
    ensure(fail.is_err(), "ai health should fail when proxy enabled")?;

    if let Some(v) = prev_http_proxy {
        std::env::set_var("HTTP_PROXY", v);
    } else {
        std::env::remove_var("HTTP_PROXY");
    }
    if let Some(v) = prev_https_proxy {
        std::env::set_var("HTTPS_PROXY", v);
    } else {
        std::env::remove_var("HTTPS_PROXY");
    }

    proxy_handle.abort();
    mock_handle.abort();
    Ok(())
}

async fn mysql_like_flow(
    db_tag: &str,
    url: &str,
    store: JobStore,
    spec: ExportSpec,
    cases: &mut Vec<PerformanceCase>,
) -> Result<(), String> {
    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(url)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("DROP TABLE IF EXISTS e2e_items_imported")
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("DROP TABLE IF EXISTS e2e_items")
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE TABLE e2e_items (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)",
    )
    .execute(&pool)
    .await
    .map_err(|e| e.to_string())?;

    let now = Utc::now().naive_utc();
    for i in 1..=25i64 {
        sqlx::query("INSERT INTO e2e_items (id, name, score, created_at) VALUES (?, ?, ?, ?)")
            .bind(i)
            .bind(format!("item-{}", i))
            .bind(i as f64 * 1.5)
            .bind(now)
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
    }

    let t_page = std::time::Instant::now();
    let page: Vec<(i64,)> = sqlx::query_as("SELECT id FROM e2e_items ORDER BY id LIMIT ? OFFSET ?")
        .bind(10i64)
        .bind(10i64)
        .fetch_all(&pool)
        .await
        .map_err(|e| e.to_string())?;
    ensure(page.len() == 10, format!("{db_tag} pagination rows != 10"))?;
    ensure(page[0].0 == 11, format!("{db_tag} pagination first id != 11"))?;
    {
        let mut labels = HashMap::new();
        labels.insert("db".to_string(), db_tag.to_string());
        push_case(
            cases,
            format!("sql_pagination_{}", db_tag),
            "sql_pagination",
            labels,
            t_page.elapsed().as_millis(),
            Some(page.len() as u64),
            None,
            vec![PerformanceStage {
                name: "query".to_string(),
                metrics: PerformanceMetrics::new(t_page.elapsed().as_millis(), Some(page.len() as u64), None),
            }],
            None,
        );
    }

    let t_export = std::time::Instant::now();
    let export_job_id = Uuid::new_v4().to_string();
    let export_job_id_for_task = export_job_id.clone();
    {
        let mut jobs = store.export_jobs.write().await;
        jobs.insert(
            export_job_id.clone(),
            ExportJob {
                status: JobStatus::Pending,
                artifacts: None,
            },
        );
    }
    let store_clone = store.clone();
    let pool_clone = pool.clone();
    let spec_clone = spec.clone();
    let db_tag_s = db_tag.to_string();
    let export_handle = tokio::spawn(async move {
        {
            let mut jobs = store_clone.export_jobs.write().await;
            if let Some(j) = jobs.get_mut(&export_job_id_for_task) {
                j.status = JobStatus::Running;
            }
        }

        let res: Result<ExportArtifacts, String> = async {
            let rows: Vec<Value> = sqlx::query(
                "SELECT id, name, score, created_at FROM e2e_items WHERE name LIKE ? AND id >= ? AND id <= ? ORDER BY id LIMIT ? OFFSET ?",
            )
            .bind(spec_clone.name_like)
            .bind(spec_clone.pk_start)
            .bind(spec_clone.pk_end)
            .bind(spec_clone.window_limit)
            .bind(spec_clone.window_offset)
            .fetch_all(&pool_clone)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| {
                let id: i64 = r.get::<i64, _>(0);
                let name: String = r.get::<String, _>(1);
                let score: f64 = r.get::<f64, _>(2);
                let created_at: chrono::NaiveDateTime = r.get::<chrono::NaiveDateTime, _>(3);
                serde_json::json!({
                    "id": id,
                    "name": name,
                    "score": score,
                    "created_at": created_at.to_string(),
                })
            })
            .collect();

            write_export_files(&db_tag_s, &export_job_id_for_task, &rows).await
        }
        .await;

        let mut jobs = store_clone.export_jobs.write().await;
        if let Some(j) = jobs.get_mut(&export_job_id_for_task) {
            match res {
                Ok(artifacts) => {
                    j.status = JobStatus::Completed;
                    j.artifacts = Some(artifacts);
                }
                Err(e) => j.status = JobStatus::Error(e),
            }
        }
    });
    let _ = export_handle.await;

    let artifacts = export_job_poll(&store, &export_job_id).await?;
    ensure(artifacts.row_count as usize <= spec.window_limit as usize, "export rows exceed window")?;

    let exported_bytes = tokio::fs::read(&artifacts.data_path)
        .await
        .map_err(|e| e.to_string())?;
    let exported_rows: Vec<Value> = serde_json::from_slice(&exported_bytes).map_err(|e| e.to_string())?;
    ensure(!exported_rows.is_empty(), "exported rows should not be empty")?;
    {
        let mut labels = HashMap::new();
        labels.insert("db".to_string(), db_tag.to_string());
        labels.insert("format".to_string(), "json".to_string());
        push_case(
            cases,
            format!("export_{}", db_tag),
            "export",
            labels,
            t_export.elapsed().as_millis(),
            Some(artifacts.row_count),
            Some(exported_bytes.len() as u64),
            vec![PerformanceStage {
                name: "export".to_string(),
                metrics: PerformanceMetrics::new(
                    t_export.elapsed().as_millis(),
                    Some(artifacts.row_count),
                    Some(exported_bytes.len() as u64),
                ),
            }],
            None,
        );
    }

    sqlx::query(
        "CREATE TABLE e2e_items_imported (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)",
    )
    .execute(&pool)
    .await
    .map_err(|e| e.to_string())?;

    let t_import = std::time::Instant::now();
    let import_job_id = Uuid::new_v4().to_string();
    let import_job_id_for_task = import_job_id.clone();
    {
        let mut jobs = store.import_jobs.write().await;
        jobs.insert(
            import_job_id.clone(),
            ImportJob {
                status: JobStatus::Pending,
                inserted: None,
            },
        );
    }
    let store_clone = store.clone();
    let pool_clone = pool.clone();
    let import_rows = exported_rows.clone();
    let import_handle = tokio::spawn(async move {
        {
            let mut jobs = store_clone.import_jobs.write().await;
            if let Some(j) = jobs.get_mut(&import_job_id_for_task) {
                j.status = JobStatus::Running;
            }
        }

        let res: Result<u64, String> = async {
            let mut inserted = 0u64;
            for v in import_rows {
                let id = v.get("id").and_then(|x| x.as_i64()).unwrap_or_default();
                let name = v.get("name").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                let score = v.get("score").and_then(|x| x.as_f64()).unwrap_or_default();
                let created_at = v
                    .get("created_at")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                sqlx::query("INSERT INTO e2e_items_imported (id, name, score, created_at) VALUES (?, ?, ?, ?)")
                    .bind(id)
                    .bind(name)
                    .bind(score)
                    .bind(created_at)
                    .execute(&pool_clone)
                    .await
                    .map_err(|e| e.to_string())?;
                inserted += 1;
            }
            Ok(inserted)
        }
        .await;

        let mut jobs = store_clone.import_jobs.write().await;
        if let Some(j) = jobs.get_mut(&import_job_id_for_task) {
            match res {
                Ok(inserted) => {
                    j.status = JobStatus::Completed;
                    j.inserted = Some(inserted);
                }
                Err(e) => j.status = JobStatus::Error(e),
            }
        }
    });
    let _ = import_handle.await;
    let inserted = import_job_poll(&store, &import_job_id).await?;
    ensure(inserted as usize == exported_rows.len(), "imported rows mismatch")?;
    {
        let mut labels = HashMap::new();
        labels.insert("db".to_string(), db_tag.to_string());
        push_case(
            cases,
            format!("import_{}", db_tag),
            "import",
            labels,
            t_import.elapsed().as_millis(),
            Some(inserted),
            Some(exported_bytes.len() as u64),
            vec![PerformanceStage {
                name: "import".to_string(),
                metrics: PerformanceMetrics::new(
                    t_import.elapsed().as_millis(),
                    Some(inserted),
                    Some(exported_bytes.len() as u64),
                ),
            }],
            None,
        );
    }

    let imported_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM e2e_items_imported")
            .fetch_one(&pool)
            .await
            .map_err(|e| e.to_string())?;
    ensure(imported_count.0 as usize == exported_rows.len(), "imported table count mismatch")?;
    Ok(())
}

async fn postgres_flow(
    db_tag: &str,
    url: &str,
    spec: ExportSpec,
    cases: &mut Vec<PerformanceCase>,
) -> Result<(), String> {
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(url)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("DROP TABLE IF EXISTS e2e_items_imported")
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("DROP TABLE IF EXISTS e2e_items")
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE TABLE e2e_items (id BIGINT PRIMARY KEY, name TEXT NOT NULL, score DOUBLE PRECISION NOT NULL, created_at TIMESTAMP NOT NULL)",
    )
    .execute(&pool)
    .await
    .map_err(|e| e.to_string())?;

    let now = Utc::now().naive_utc();
    for i in 1..=25i64 {
        sqlx::query("INSERT INTO e2e_items (id, name, score, created_at) VALUES ($1, $2, $3, $4)")
            .bind(i)
            .bind(format!("item-{}", i))
            .bind(i as f64 * 1.5)
            .bind(now)
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
    }

    let t_page = std::time::Instant::now();
    let page: Vec<(i64,)> = sqlx::query_as("SELECT id FROM e2e_items ORDER BY id LIMIT $1 OFFSET $2")
        .bind(10i64)
        .bind(10i64)
        .fetch_all(&pool)
        .await
        .map_err(|e| e.to_string())?;
    ensure(page.len() == 10, format!("{db_tag} pagination rows != 10"))?;
    ensure(page[0].0 == 11, format!("{db_tag} pagination first id != 11"))?;
    {
        let mut labels = HashMap::new();
        labels.insert("db".to_string(), db_tag.to_string());
        push_case(
            cases,
            format!("sql_pagination_{}", db_tag),
            "sql_pagination",
            labels,
            t_page.elapsed().as_millis(),
            Some(page.len() as u64),
            None,
            vec![PerformanceStage {
                name: "query".to_string(),
                metrics: PerformanceMetrics::new(t_page.elapsed().as_millis(), Some(page.len() as u64), None),
            }],
            None,
        );
    }

    let t_export = std::time::Instant::now();
    let rows: Vec<Value> = sqlx::query(
        "SELECT id, name, score, created_at FROM e2e_items WHERE name LIKE $1 AND id >= $2 AND id <= $3 ORDER BY id LIMIT $4 OFFSET $5",
    )
    .bind(spec.name_like)
    .bind(spec.pk_start)
    .bind(spec.pk_end)
    .bind(spec.window_limit)
    .bind(spec.window_offset)
    .fetch_all(&pool)
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .map(|r| {
        let id: i64 = r.get::<i64, _>(0);
        let name: String = r.get::<String, _>(1);
        let score: f64 = r.get::<f64, _>(2);
        let created_at: chrono::NaiveDateTime = r.get::<chrono::NaiveDateTime, _>(3);
        serde_json::json!({
            "id": id,
            "name": name,
            "score": score,
            "created_at": created_at.to_string(),
        })
    })
    .collect();
    ensure(!rows.is_empty(), format!("{db_tag} export rows empty"))?;
    {
        let bytes = serde_json::to_vec(&rows).map(|b| b.len() as u64).unwrap_or(0);
        let mut labels = HashMap::new();
        labels.insert("db".to_string(), db_tag.to_string());
        labels.insert("format".to_string(), "json".to_string());
        push_case(
            cases,
            format!("export_{}", db_tag),
            "export",
            labels,
            t_export.elapsed().as_millis(),
            Some(rows.len() as u64),
            Some(bytes),
            vec![PerformanceStage {
                name: "export".to_string(),
                metrics: PerformanceMetrics::new(t_export.elapsed().as_millis(), Some(rows.len() as u64), Some(bytes)),
            }],
            None,
        );
    }
    Ok(())
}

async fn sqlite_flow(
    db_tag: &str,
    url: &str,
    spec: ExportSpec,
    cases: &mut Vec<PerformanceCase>,
) -> Result<(), String> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("DROP TABLE IF EXISTS e2e_items_imported")
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("DROP TABLE IF EXISTS e2e_items")
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE TABLE e2e_items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL NOT NULL, created_at TEXT NOT NULL)",
    )
    .execute(&pool)
    .await
    .map_err(|e| e.to_string())?;

    let now = Utc::now().to_rfc3339();
    for i in 1..=25i64 {
        sqlx::query("INSERT INTO e2e_items (id, name, score, created_at) VALUES (?, ?, ?, ?)")
            .bind(i)
            .bind(format!("item-{}", i))
            .bind(i as f64 * 1.5)
            .bind(now.clone())
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
    }

    let t_page = std::time::Instant::now();
    let page: Vec<(i64,)> = sqlx::query_as("SELECT id FROM e2e_items ORDER BY id LIMIT ? OFFSET ?")
        .bind(10i64)
        .bind(10i64)
        .fetch_all(&pool)
        .await
        .map_err(|e| e.to_string())?;
    ensure(page.len() == 10, format!("{db_tag} pagination rows != 10"))?;
    ensure(page[0].0 == 11, format!("{db_tag} pagination first id != 11"))?;
    {
        let mut labels = HashMap::new();
        labels.insert("db".to_string(), db_tag.to_string());
        push_case(
            cases,
            format!("sql_pagination_{}", db_tag),
            "sql_pagination",
            labels,
            t_page.elapsed().as_millis(),
            Some(page.len() as u64),
            None,
            vec![PerformanceStage {
                name: "query".to_string(),
                metrics: PerformanceMetrics::new(t_page.elapsed().as_millis(), Some(page.len() as u64), None),
            }],
            None,
        );
    }

    let t_export = std::time::Instant::now();
    let rows: Vec<Value> = sqlx::query(
        "SELECT id, name, score, created_at FROM e2e_items WHERE name LIKE ? AND id >= ? AND id <= ? ORDER BY id LIMIT ? OFFSET ?",
    )
    .bind(spec.name_like)
    .bind(spec.pk_start)
    .bind(spec.pk_end)
    .bind(spec.window_limit)
    .bind(spec.window_offset)
    .fetch_all(&pool)
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .map(|r| {
        let id: i64 = r.get::<i64, _>(0);
        let name: String = r.get::<String, _>(1);
        let score: f64 = r.get::<f64, _>(2);
        let created_at: String = r.get::<String, _>(3);
        serde_json::json!({
            "id": id,
            "name": name,
            "score": score,
            "created_at": created_at,
        })
    })
    .collect();
    ensure(!rows.is_empty(), format!("{db_tag} export rows empty"))?;
    {
        let bytes = serde_json::to_vec(&rows).map(|b| b.len() as u64).unwrap_or(0);
        let mut labels = HashMap::new();
        labels.insert("db".to_string(), db_tag.to_string());
        labels.insert("format".to_string(), "json".to_string());
        push_case(
            cases,
            format!("export_{}", db_tag),
            "export",
            labels,
            t_export.elapsed().as_millis(),
            Some(rows.len() as u64),
            Some(bytes),
            vec![PerformanceStage {
                name: "export".to_string(),
                metrics: PerformanceMetrics::new(t_export.elapsed().as_millis(), Some(rows.len() as u64), Some(bytes)),
            }],
            None,
        );
    }
    Ok(())
}

async fn mysql_sync_flow(
    source_url: &str,
    target_url: &str,
    cases: &mut Vec<PerformanceCase>,
) -> Result<(), String> {
    let source = DbClient::new(source_url).await.map_err(|e| e.to_string())?;
    let target = DbClient::new(target_url).await.map_err(|e| e.to_string())?;

    let ddl = "CREATE TABLE e2e_sync_items (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)";
    sqlx::query("DROP TABLE IF EXISTS e2e_sync_items")
        .execute(&source.pool)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("DROP TABLE IF EXISTS e2e_sync_items")
        .execute(&target.pool)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(ddl)
        .execute(&source.pool)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(ddl)
        .execute(&target.pool)
        .await
        .map_err(|e| e.to_string())?;

    let now = Utc::now().naive_utc();
    for i in 1..=500i64 {
        sqlx::query("INSERT INTO e2e_sync_items (id, name, score, created_at) VALUES (?, ?, ?, ?)")
            .bind(i)
            .bind(format!("item-{}", i))
            .bind(i as f64)
            .bind(now)
            .execute(&source.pool)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query("INSERT INTO e2e_sync_items (id, name, score, created_at) VALUES (?, ?, ?, ?)")
            .bind(i)
            .bind(format!("item-{}", i))
            .bind(i as f64)
            .bind(now)
            .execute(&target.pool)
            .await
            .map_err(|e| e.to_string())?;
    }

    sqlx::query("UPDATE e2e_sync_items SET score = score + 1 WHERE id % 50 = 0")
        .execute(&target.pool)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("DELETE FROM e2e_sync_items WHERE id % 77 = 0")
        .execute(&target.pool)
        .await
        .map_err(|e| e.to_string())?;
    for i in 501..=510i64 {
        sqlx::query("INSERT INTO e2e_sync_items (id, name, score, created_at) VALUES (?, ?, ?, ?)")
            .bind(i)
            .bind(format!("extra-{}", i))
            .bind(i as f64)
            .bind(now)
            .execute(&target.pool)
            .await
            .map_err(|e| e.to_string())?;
    }

    let t_compare = std::time::Instant::now();
    let compare = MySqlDataSyncEngine::compare(&source, &target, "e2e_sync_items", "id", 50)
        .await
        .map_err(|e| e.to_string())?;
    let compare_ms = t_compare.elapsed().as_millis();

    let t_preview = std::time::Instant::now();
    let preview = MySqlDataSyncEngine::preview(
        &source,
        &target,
        &compare,
        SyncMode::Mirror,
        2000,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;
    let preview_ms = t_preview.elapsed().as_millis();

    let t_deploy = std::time::Instant::now();
    let affected = MySqlDataSyncEngine::deploy(&target, &preview.statements, |_c, _t| {})
        .await
        .map_err(|e| e.to_string())?;
    let deploy_ms = t_deploy.elapsed().as_millis();

    let t_verify = std::time::Instant::now();
    let verify = MySqlDataSyncEngine::compare(&source, &target, "e2e_sync_items", "id", 50)
        .await
        .map_err(|e| e.to_string())?;
    let verify_ms = t_verify.elapsed().as_millis();
    ensure(verify.different_chunks == 0, "sync verify should have zero diff")?;

    let mut labels = HashMap::new();
    labels.insert("source".to_string(), "mysql".to_string());
    labels.insert("target".to_string(), "mariadb".to_string());
    labels.insert("table".to_string(), "e2e_sync_items".to_string());
    labels.insert("mode".to_string(), "mirror".to_string());

    let stages = vec![
        PerformanceStage {
            name: "compare".to_string(),
            metrics: PerformanceMetrics::new(compare_ms, None, None),
        },
        PerformanceStage {
            name: "preview".to_string(),
            metrics: PerformanceMetrics::new(preview_ms, None, None),
        },
        PerformanceStage {
            name: "deploy".to_string(),
            metrics: PerformanceMetrics::new(deploy_ms, Some(affected), None),
        },
        PerformanceStage {
            name: "verify".to_string(),
            metrics: PerformanceMetrics::new(verify_ms, None, None),
        },
    ];

    let duration_ms = compare_ms + preview_ms + deploy_ms + verify_ms;
    let extra = serde_json::json!({
        "compare_chunks": compare.chunks.len(),
        "different_chunks": compare.different_chunks,
        "preview": {
            "insert": preview.diff.insert_count,
            "update": preview.diff.update_count,
            "delete": preview.diff.delete_count,
            "statements": preview.statements.len(),
            "truncated": preview.truncated
        },
        "affected_rows": affected,
        "verify_different_chunks": verify.different_chunks,
        "verify_ms": verify_ms
    });

    push_case(
        cases,
        "sync_mysql_mariadb_e2e_sync_items".to_string(),
        "sync",
        labels,
        duration_ms,
        Some(affected),
        None,
        stages,
        Some(extra),
    );

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = RunConfig::from_env();
    let store = JobStore::default();
    let spec = ExportSpec {
        name_like: "%item-%".to_string(),
        pk_start: 5,
        pk_end: 20,
        window_limit: 7,
        window_offset: 3,
    };

    let mut failures = Vec::new();
    let mut cases: Vec<PerformanceCase> = Vec::new();

    if let Err(e) = mysql_like_flow("mysql", &config.mysql_url, store.clone(), spec.clone(), &mut cases).await {
        failures.push(format!("mysql: {}", e));
    }
    if let Err(e) =
        mysql_like_flow("mariadb", &config.mariadb_url, store.clone(), spec.clone(), &mut cases).await
    {
        failures.push(format!("mariadb: {}", e));
    }
    if let Err(e) = postgres_flow("postgres", &config.postgres_url, spec.clone(), &mut cases).await {
        failures.push(format!("postgres: {}", e));
    }
    if let Err(e) = sqlite_flow("sqlite", &config.sqlite_url, spec.clone(), &mut cases).await {
        failures.push(format!("sqlite: {}", e));
    }
    if let Err(e) = mysql_sync_flow(&config.mysql_url, &config.mariadb_url, &mut cases).await {
        failures.push(format!("sync: {}", e));
    }
    if let Err(e) = ai_proxy_failure_split_e2e().await {
        failures.push(format!("proxy: {}", e));
    }

    let report_path = std::env::var("REPORT_PATH").unwrap_or_else(|_| {
        let mut p = std::env::temp_dir();
        p.push("local-ai-sql-e2e-performance.json");
        p.to_string_lossy().to_string()
    });
    let mut report = PerformanceReport::new(cases);
    report.meta = Some(serde_json::json!({ "failures": failures.clone() }));
    tokio::fs::write(&report_path, serde_json::to_vec_pretty(&report)?).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("REPORT_PATH={}", report_path);

    if failures.is_empty() {
        println!("E2E OK");
        Ok(())
    } else {
        eprintln!("E2E FAILED:\n{}", failures.join("\n"));
        std::process::exit(1);
    }
}
