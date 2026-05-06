use crate::db::DbClient;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Executor;
use std::time::Instant;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadgenTier {
    M1,
    M10,
    M100,
}

impl LoadgenTier {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "1m" | "1_000_000" | "1000000" => Some(Self::M1),
            "10m" | "10_000_000" | "10000000" => Some(Self::M10),
            "100m" | "100_000_000" | "100000000" => Some(Self::M100),
            _ => None,
        }
    }

    pub fn rows_users(self) -> u64 {
        match self {
            Self::M1 => 1_000_000,
            Self::M10 => 10_000_000,
            Self::M100 => 100_000_000,
        }
    }

    pub fn rows_orders(self) -> u64 {
        match self {
            Self::M1 => 1_000_000,
            Self::M10 => 10_000_000,
            Self::M100 => 100_000_000,
        }
    }

    pub fn rows_events(self) -> u64 {
        match self {
            Self::M1 => 1_000_000,
            Self::M10 => 10_000_000,
            Self::M100 => 100_000_000,
        }
    }

    pub fn rows_kv(self) -> u64 {
        match self {
            Self::M1 => 1_000_000,
            Self::M10 => 10_000_000,
            Self::M100 => 100_000_000,
        }
    }

    pub fn rows_files(self) -> u64 {
        match self {
            Self::M1 => 100_000,
            Self::M10 => 1_000_000,
            Self::M100 => 10_000_000,
        }
    }

    pub fn rows_map(self) -> serde_json::Value {
        json!({
            "users": self.rows_users(),
            "orders": self.rows_orders(),
            "events": self.rows_events(),
            "kv_hotspot": self.rows_kv(),
            "files": self.rows_files()
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergeProfile {
    Mirror,
    UpsertOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadgenConfig {
    pub tier: LoadgenTier,
    pub reset: bool,
    pub seed: u64,
    pub batch: u64,
    pub diverge: Option<DivergeProfile>,
}

impl Default for LoadgenConfig {
    fn default() -> Self {
        Self {
            tier: LoadgenTier::M1,
            reset: false,
            seed: 1,
            batch: 1000,
            diverge: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadgenReport {
    pub tier: String,
    pub reset: bool,
    pub diverge: Option<DivergeProfile>,
    pub seed: u64,
    pub batch: u64,
    pub rows: serde_json::Value,
    pub before_rows: serde_json::Value,
    pub after_rows: serde_json::Value,
    pub elapsed_ms: u128,
}

#[derive(Clone, Copy)]
struct XorShift64 {
    s: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let seed = if seed == 0 { 88172645463393265 } else { seed };
        Self { s: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.s = x;
        x
    }

    fn range(&mut self, start: u64, end_exclusive: u64) -> u64 {
        if end_exclusive <= start {
            return start;
        }
        start + (self.next_u64() % (end_exclusive - start))
    }
}

fn escape_sql(s: &str) -> String {
    s.replace('\'', "''")
}

fn base36(mut x: u64, min_len: usize) -> String {
    const ALPH: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        let idx = (x % 36) as usize;
        out.push(ALPH[idx] as char);
        x /= 36;
        if x == 0 {
            break;
        }
    }
    while out.len() < min_len {
        out.push('0');
    }
    out.iter().rev().collect()
}

fn make_email(rng: &mut XorShift64, id: u64) -> String {
    let dom = match rng.range(0, 4) {
        0 => "example.com",
        1 => "corp.local",
        2 => "mail.test",
        _ => "sample.org",
    };
    format!("u{}-{}@{}", id, base36(rng.next_u64(), 6), dom)
}

fn make_name(rng: &mut XorShift64) -> String {
    let a = base36(rng.next_u64(), 4);
    let b = base36(rng.next_u64(), 4);
    format!("{} {}", a, b)
}

fn make_payload(rng: &mut XorShift64, id: u64) -> String {
    let v = json!({
        "id": id,
        "flags": [rng.range(0, 10), rng.range(0, 10)],
        "tag": base36(rng.next_u64(), 8),
        "score": (rng.range(0, 10_000) as f64) / 100.0
    });
    v.to_string()
}

async fn exec(db: &DbClient, sql: &str) -> Result<(), AppError> {
    db.pool
        .execute(sql)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(())
}

async fn create_schema(db: &DbClient) -> Result<(), AppError> {
    exec(
        db,
        r#"
CREATE TABLE IF NOT EXISTS users (
  id BIGINT NOT NULL PRIMARY KEY,
  email VARCHAR(255) NOT NULL,
  name VARCHAR(128) NOT NULL,
  status TINYINT NOT NULL,
  balance DECIMAL(18,2) NOT NULL,
  payload JSON,
  created_at DATETIME NOT NULL,
  updated_at DATETIME NOT NULL,
  KEY idx_users_updated_at(updated_at),
  UNIQUE KEY uq_users_email(email)
) ENGINE=InnoDB
"#,
    )
    .await?;

    exec(
        db,
        r#"
CREATE TABLE IF NOT EXISTS orders (
  id BIGINT NOT NULL PRIMARY KEY,
  user_id BIGINT NOT NULL,
  amount DECIMAL(18,2) NOT NULL,
  note TEXT,
  created_at DATETIME NOT NULL,
  updated_at DATETIME NOT NULL,
  KEY idx_orders_user_id(user_id),
  KEY idx_orders_created_at(created_at)
) ENGINE=InnoDB
"#,
    )
    .await?;

    exec(
        db,
        r#"
CREATE TABLE IF NOT EXISTS events (
  id BIGINT NOT NULL PRIMARY KEY,
  user_id BIGINT NOT NULL,
  event_type VARCHAR(32) NOT NULL,
  c1 INT NOT NULL, c2 INT NOT NULL, c3 INT NOT NULL, c4 INT NOT NULL, c5 INT NOT NULL,
  c6 INT NOT NULL, c7 INT NOT NULL, c8 INT NOT NULL, c9 INT NOT NULL, c10 INT NOT NULL,
  v1 VARCHAR(64) NOT NULL, v2 VARCHAR(64) NOT NULL, v3 VARCHAR(64) NOT NULL, v4 VARCHAR(64) NOT NULL, v5 VARCHAR(64) NOT NULL,
  created_at DATETIME NOT NULL,
  updated_at DATETIME NOT NULL,
  KEY idx_events_user_id(user_id),
  KEY idx_events_updated_at(updated_at)
) ENGINE=InnoDB
"#,
    )
    .await?;

    exec(
        db,
        r#"
CREATE TABLE IF NOT EXISTS kv_hotspot (
  id BIGINT NOT NULL PRIMARY KEY,
  tenant_id INT NOT NULL,
  k VARCHAR(128) NOT NULL,
  v VARCHAR(512) NOT NULL,
  updated_at DATETIME NOT NULL,
  UNIQUE KEY uq_kv_tenant_key(tenant_id, k),
  KEY idx_kv_updated_at(updated_at)
) ENGINE=InnoDB
"#,
    )
    .await?;

    exec(
        db,
        r#"
CREATE TABLE IF NOT EXISTS files (
  id BIGINT NOT NULL PRIMARY KEY,
  sha256 CHAR(64) NOT NULL,
  blob_hex MEDIUMTEXT NOT NULL,
  updated_at DATETIME NOT NULL,
  KEY idx_files_updated_at(updated_at)
) ENGINE=InnoDB
"#,
    )
    .await?;

    Ok(())
}

async fn reset_tables(db: &DbClient) -> Result<(), AppError> {
    exec(db, "TRUNCATE TABLE users").await?;
    exec(db, "TRUNCATE TABLE orders").await?;
    exec(db, "TRUNCATE TABLE events").await?;
    exec(db, "TRUNCATE TABLE kv_hotspot").await?;
    exec(db, "TRUNCATE TABLE files").await?;
    Ok(())
}

async fn bulk_insert_users(
    db: &DbClient,
    start_id: u64,
    rows: u64,
    batch: u64,
    seed: u64,
) -> Result<(), AppError> {
    let mut rng = XorShift64::new(seed ^ 0x9e3779b97f4a7c15);
    let mut i = 0u64;
    while i < rows {
        let n = (rows - i).min(batch);
        let mut sql =
            String::from("INSERT INTO users (id,email,name,status,balance,payload,created_at,updated_at) VALUES ");
        for j in 0..n {
            let id = start_id + i + j;
            let email = escape_sql(&make_email(&mut rng, id));
            let name = escape_sql(&make_name(&mut rng));
            let status = rng.range(0, 5) as i64;
            let balance = (rng.range(0, 1_000_000) as f64) / 100.0;
            let payload = escape_sql(&make_payload(&mut rng, id));
            let day = 1 + (id % 28);
            let created_at = format!("2025-01-{:02} 12:{:02}:{:02}", day, id % 60, (id / 60) % 60);
            let updated_at = format!(
                "2026-01-{:02} 12:{:02}:{:02}",
                day,
                (id * 7) % 60,
                (id * 11) % 60
            );
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({},'{}','{}',{},{:.2},'{}','{}','{}')",
                id, email, name, status, balance, payload, created_at, updated_at
            ));
        }
        exec(db, &sql).await?;
        i += n;
    }
    Ok(())
}

async fn bulk_insert_orders(
    db: &DbClient,
    start_id: u64,
    rows: u64,
    max_user_id: u64,
    batch: u64,
    seed: u64,
) -> Result<(), AppError> {
    let mut rng = XorShift64::new(seed ^ 0x243f6a8885a308d3);
    let mut i = 0u64;
    while i < rows {
        let n = (rows - i).min(batch);
        let mut sql =
            String::from("INSERT INTO orders (id,user_id,amount,note,created_at,updated_at) VALUES ");
        for j in 0..n {
            let id = start_id + i + j;
            let user_id = 1 + (rng.range(0, max_user_id.max(1)) as i64);
            let amount = (rng.range(0, 10_000_000) as f64) / 100.0;
            let note = escape_sql(&format!("note-{}-{}", id, base36(rng.next_u64(), 10)));
            let day = 1 + (id % 28);
            let created_at = format!("2025-02-{:02} 08:{:02}:{:02}", day, id % 60, (id / 60) % 60);
            let updated_at = format!(
                "2026-02-{:02} 08:{:02}:{:02}",
                day,
                (id * 5) % 60,
                (id * 13) % 60
            );
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({}, {}, {:.2}, '{}', '{}', '{}')",
                id, user_id, amount, note, created_at, updated_at
            ));
        }
        exec(db, &sql).await?;
        i += n;
    }
    Ok(())
}

async fn bulk_insert_events(
    db: &DbClient,
    start_id: u64,
    rows: u64,
    max_user_id: u64,
    batch: u64,
    seed: u64,
) -> Result<(), AppError> {
    let mut rng = XorShift64::new(seed ^ 0x13198a2e03707344);
    let types = ["click", "view", "purchase", "refund", "login", "logout"];
    let mut i = 0u64;
    while i < rows {
        let n = (rows - i).min(batch);
        let mut sql = String::from("INSERT INTO events (id,user_id,event_type,c1,c2,c3,c4,c5,c6,c7,c8,c9,c10,v1,v2,v3,v4,v5,created_at,updated_at) VALUES ");
        for j in 0..n {
            let id = start_id + i + j;
            let user_id = 1 + rng.range(0, max_user_id.max(1));
            let event_type = types[rng.range(0, types.len() as u64) as usize];
            let c = (0..10)
                .map(|_| rng.range(0, 10_000) as i64)
                .collect::<Vec<_>>();
            let v = (0..5)
                .map(|_| escape_sql(&base36(rng.next_u64(), 12)))
                .collect::<Vec<_>>();
            let day = 1 + (id % 28);
            let created_at = format!("2025-03-{:02} 10:{:02}:{:02}", day, id % 60, (id / 60) % 60);
            let updated_at = format!(
                "2026-03-{:02} 10:{:02}:{:02}",
                day,
                (id * 3) % 60,
                (id * 17) % 60
            );
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({}, {}, '{}', {},{},{},{},{},{},{},{},{},{}, '{}','{}','{}','{}','{}', '{}','{}')",
                id,
                user_id,
                escape_sql(event_type),
                c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8], c[9],
                v[0], v[1], v[2], v[3], v[4],
                created_at,
                updated_at
            ));
        }
        exec(db, &sql).await?;
        i += n;
    }
    Ok(())
}

async fn bulk_insert_kv(
    db: &DbClient,
    start_id: u64,
    rows: u64,
    batch: u64,
    seed: u64,
) -> Result<(), AppError> {
    let mut rng = XorShift64::new(seed ^ 0xa4093822299f31d0);
    let mut i = 0u64;
    while i < rows {
        let n = (rows - i).min(batch);
        let mut sql = String::from("INSERT INTO kv_hotspot (id,tenant_id,k,v,updated_at) VALUES ");
        for j in 0..n {
            let id = start_id + i + j;
            let tenant_id = if rng.range(0, 100) < 80 {
                1
            } else {
                (1 + rng.range(0, 500)) as i64
            };
            let k = escape_sql(&format!("k-{}-{}", tenant_id, base36(rng.next_u64(), 10)));
            let v = escape_sql(&format!("v-{}-{}", id, base36(rng.next_u64(), 24)));
            let day = 1 + (id % 28);
            let updated_at = format!("2026-04-{:02} 09:{:02}:{:02}", day, id % 60, (id / 60) % 60);
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, '{}', '{}', '{}')", id, tenant_id, k, v, updated_at));
        }
        exec(db, &sql).await?;
        i += n;
    }
    Ok(())
}

async fn bulk_insert_files(
    db: &DbClient,
    start_id: u64,
    rows: u64,
    batch: u64,
    seed: u64,
) -> Result<(), AppError> {
    let mut rng = XorShift64::new(seed ^ 0x082efa98ec4e6c89);
    let mut i = 0u64;
    while i < rows {
        let n = (rows - i).min(batch);
        let mut sql = String::from("INSERT INTO files (id,sha256,blob_hex,updated_at) VALUES ");
        for j in 0..n {
            let id = start_id + i + j;
            let sha = base36(rng.next_u64(), 32) + &base36(rng.next_u64(), 32);
            let blob_hex = base36(rng.next_u64(), 64) + &base36(rng.next_u64(), 64);
            let day = 1 + (id % 28);
            let updated_at = format!("2026-05-{:02} 11:{:02}:{:02}", day, id % 60, (id / 60) % 60);
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({}, '{}', '{}', '{}')",
                id,
                escape_sql(&sha),
                escape_sql(&blob_hex),
                updated_at
            ));
        }
        exec(db, &sql).await?;
        i += n;
    }
    Ok(())
}

async fn diverge_target_mirror(target: &DbClient, tier: LoadgenTier) -> Result<(), AppError> {
    exec(target, "DELETE FROM users WHERE id % 100 = 0").await?;
    exec(
        target,
        "UPDATE users SET status = (status + 1) % 5, updated_at = NOW() WHERE id % 50 = 0",
    )
    .await?;
    let ins_users = tier.rows_users() / 100;
    if ins_users > 0 {
        let max_id: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM users")
            .fetch_one(&target.pool)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        let start = (max_id as u64) + 1;
        bulk_insert_users(target, start, ins_users, 1000, 0xdeadbeef).await?;
    }

    exec(target, "DELETE FROM orders WHERE id % 200 = 0").await?;
    exec(
        target,
        "UPDATE orders SET amount = amount + 1.23, updated_at = NOW() WHERE id % 77 = 0",
    )
    .await?;

    exec(target, "DELETE FROM events WHERE id % 250 = 0").await?;
    exec(
        target,
        "UPDATE events SET c1 = c1 + 7, updated_at = NOW() WHERE id % 99 = 0",
    )
    .await?;

    exec(
        target,
        "UPDATE kv_hotspot SET v = CONCAT(v, '-u'), updated_at = NOW() WHERE id % 33 = 0",
    )
    .await?;

    exec(target, "UPDATE files SET updated_at = NOW() WHERE id % 17 = 0").await?;
    Ok(())
}

async fn diverge_target_upsert_only(target: &DbClient, tier: LoadgenTier) -> Result<(), AppError> {
    exec(target, "DELETE FROM users WHERE id % 100 = 0").await?;
    exec(
        target,
        "UPDATE users SET status = (status + 1) % 5, updated_at = NOW() WHERE id % 50 = 0",
    )
    .await?;
    let _ = tier;

    exec(target, "DELETE FROM orders WHERE id % 200 = 0").await?;
    exec(
        target,
        "UPDATE orders SET amount = amount + 1.23, updated_at = NOW() WHERE id % 77 = 0",
    )
    .await?;

    exec(target, "DELETE FROM events WHERE id % 250 = 0").await?;
    exec(
        target,
        "UPDATE events SET c1 = c1 + 7, updated_at = NOW() WHERE id % 99 = 0",
    )
    .await?;

    exec(
        target,
        "UPDATE kv_hotspot SET v = CONCAT(v, '-u'), updated_at = NOW() WHERE id % 33 = 0",
    )
    .await?;

    exec(target, "UPDATE files SET updated_at = NOW() WHERE id % 17 = 0").await?;
    Ok(())
}

async fn table_count(db: &DbClient, table: &str) -> Result<u64, AppError> {
    let q = format!("SELECT COUNT(*) FROM {}", table);
    let v: i64 = sqlx::query_scalar(&q).fetch_one(&db.pool).await?;
    Ok(v.max(0) as u64)
}

async fn table_max_id(db: &DbClient, table: &str) -> Result<u64, AppError> {
    let q = format!("SELECT COALESCE(MAX(id), 0) FROM {}", table);
    let v: i64 = sqlx::query_scalar(&q).fetch_one(&db.pool).await?;
    Ok(v.max(0) as u64)
}

async fn fetch_all_counts(db: &DbClient) -> Result<serde_json::Value, AppError> {
    Ok(json!({
        "users": table_count(db, "users").await?,
        "orders": table_count(db, "orders").await?,
        "events": table_count(db, "events").await?,
        "kv_hotspot": table_count(db, "kv_hotspot").await?,
        "files": table_count(db, "files").await?
    }))
}

async fn ensure_min_rows(
    db: &DbClient,
    tier: LoadgenTier,
    batch: u64,
    seed: u64,
) -> Result<(), AppError> {
    let desired_users = tier.rows_users();
    let desired_orders = tier.rows_orders();
    let desired_events = tier.rows_events();
    let desired_kv = tier.rows_kv();
    let desired_files = tier.rows_files();

    let cur_users = table_count(db, "users").await?;
    if cur_users < desired_users {
        let start = table_max_id(db, "users").await? + 1;
        bulk_insert_users(db, start, desired_users - cur_users, batch, seed).await?;
    }

    let cur_orders = table_count(db, "orders").await?;
    if cur_orders < desired_orders {
        let start = table_max_id(db, "orders").await? + 1;
        bulk_insert_orders(
            db,
            start,
            desired_orders - cur_orders,
            desired_users,
            batch,
            seed,
        )
        .await?;
    }

    let cur_events = table_count(db, "events").await?;
    if cur_events < desired_events {
        let start = table_max_id(db, "events").await? + 1;
        bulk_insert_events(
            db,
            start,
            desired_events - cur_events,
            desired_users,
            batch,
            seed,
        )
        .await?;
    }

    let cur_kv = table_count(db, "kv_hotspot").await?;
    if cur_kv < desired_kv {
        let start = table_max_id(db, "kv_hotspot").await? + 1;
        bulk_insert_kv(db, start, desired_kv - cur_kv, batch, seed).await?;
    }

    let cur_files = table_count(db, "files").await?;
    if cur_files < desired_files {
        let start = table_max_id(db, "files").await? + 1;
        bulk_insert_files(db, start, desired_files - cur_files, batch, seed).await?;
    }

    Ok(())
}

pub struct LoadgenEngine;

impl LoadgenEngine {
    pub async fn ensure_schema(source: &DbClient, target: &DbClient) -> Result<(), AppError> {
        create_schema(source).await?;
        create_schema(target).await?;
        Ok(())
    }

    pub async fn reset_all(source: &DbClient, target: &DbClient) -> Result<(), AppError> {
        reset_tables(source).await?;
        reset_tables(target).await?;
        Ok(())
    }

    pub async fn diverge_target(
        target: &DbClient,
        tier: LoadgenTier,
        profile: DivergeProfile,
    ) -> Result<(), AppError> {
        match profile {
            DivergeProfile::Mirror => diverge_target_mirror(target, tier).await,
            DivergeProfile::UpsertOnly => diverge_target_upsert_only(target, tier).await,
        }
    }

    pub async fn run(
        source: &DbClient,
        target: &DbClient,
        config: LoadgenConfig,
    ) -> Result<LoadgenReport, AppError> {
        let t0 = Instant::now();

        Self::ensure_schema(source, target).await?;
        if config.reset {
            Self::reset_all(source, target).await?;
        }

        let before_rows = json!({
            "source": fetch_all_counts(source).await?,
            "target": fetch_all_counts(target).await?
        });

        ensure_min_rows(source, config.tier, config.batch, config.seed).await?;
        ensure_min_rows(target, config.tier, config.batch, config.seed).await?;

        if let Some(profile) = config.diverge {
            Self::diverge_target(target, config.tier, profile).await?;
        }

        let after_rows = json!({
            "source": fetch_all_counts(source).await?,
            "target": fetch_all_counts(target).await?
        });

        Ok(LoadgenReport {
            tier: match config.tier {
                LoadgenTier::M1 => "1m".to_string(),
                LoadgenTier::M10 => "10m".to_string(),
                LoadgenTier::M100 => "100m".to_string(),
            },
            reset: config.reset,
            diverge: config.diverge,
            seed: config.seed,
            batch: config.batch,
            rows: config.tier.rows_map(),
            before_rows,
            after_rows,
            elapsed_ms: t0.elapsed().as_millis(),
        })
    }
}
