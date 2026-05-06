use core_lib::db::DbClient;
use core_lib::mysql_sync::{MySqlDataSyncEngine, SyncMode};
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source_url = std::env::var("SOURCE_DB_URL")
        .or_else(|_| std::env::var("SOURCE_URL"))
        .expect("Missing SOURCE_DB_URL");
    let target_url = std::env::var("TARGET_DB_URL")
        .or_else(|_| std::env::var("TARGET_URL"))
        .expect("Missing TARGET_DB_URL");

    let table = std::env::var("TABLE").expect("Missing TABLE");
    let primary_key = std::env::var("PRIMARY_KEY").unwrap_or_else(|_| "id".to_string());
    let mode = std::env::var("MODE").unwrap_or_else(|_| "mirror".to_string());
    let chunk_size: usize = std::env::var("CHUNK_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let max_rows: usize = std::env::var("MAX_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20000);

    let mode = match mode.to_lowercase().as_str() {
        "mirror" => SyncMode::Mirror,
        "upsert_only" | "upsert-only" | "upsert" => SyncMode::UpsertOnly,
        other => panic!("Invalid MODE: {}", other),
    };

    let t0 = Instant::now();
    let source = DbClient::new(&source_url).await?;
    let target = DbClient::new(&target_url).await?;
    let connect_ms = t0.elapsed().as_millis();

    let t1 = Instant::now();
    let compare = MySqlDataSyncEngine::compare(&source, &target, &table, &primary_key, chunk_size).await?;
    let compare_ms = t1.elapsed().as_millis();

    let t2 = Instant::now();
    let preview =
        MySqlDataSyncEngine::preview(&source, &target, &compare, mode, max_rows, None).await?;
    let preview_ms = t2.elapsed().as_millis();

    let out = serde_json::json!({
        "connect_ms": connect_ms,
        "compare_ms": compare_ms,
        "preview_ms": preview_ms,
        "table": table,
        "primary_key": primary_key,
        "chunk_size": compare.chunk_size,
        "chunks": compare.chunks.len(),
        "different_chunks": compare.different_chunks,
        "diff": {
            "insert": preview.diff.insert_count,
            "update": preview.diff.update_count,
            "delete": preview.diff.delete_count,
            "truncated": preview.truncated,
            "statements": preview.statements.len()
        }
    });

    println!("{}", serde_json::to_string_pretty(&out)?);
    eprintln!("Usage tip: For end-to-end HTTP+engine benchmark, use `mysql_sync_runner` (BASE_URL/SOURCE_DB_ID/TARGET_DB_ID).");
    Ok(())
}
