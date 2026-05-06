fn parse_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "on"))
        .unwrap_or(false)
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    let key_eq = format!("{}=", key);
    for a in args {
        if a.starts_with(&key_eq) {
            return Some(a[key_eq.len()..].to_string());
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    let source_url = std::env::var("SOURCE_DB_URL")
        .or_else(|_| std::env::var("SOURCE_URL"))
        .expect("Missing SOURCE_DB_URL");
    let target_url = std::env::var("TARGET_DB_URL")
        .or_else(|_| std::env::var("TARGET_URL"))
        .expect("Missing TARGET_DB_URL");

    let tier_str = arg_value(&args, "--tier").unwrap_or_else(|| "1m".to_string());
    let tier = core_lib::loadgen::LoadgenTier::parse(&tier_str)
        .unwrap_or(core_lib::loadgen::LoadgenTier::M1);
    let reset = parse_flag("RESET");
    let diverge = parse_flag("DIVERGE");
    let seed: u64 = std::env::var("SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let batch: u64 = std::env::var("BATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    let diverge = if diverge {
        Some(core_lib::loadgen::DivergeProfile::Mirror)
    } else {
        None
    };

    let source = core_lib::db::DbClient::new(&source_url).await?;
    let target = core_lib::db::DbClient::new(&target_url).await?;

    let report = core_lib::loadgen::LoadgenEngine::run(
        &source,
        &target,
        core_lib::loadgen::LoadgenConfig {
            tier,
            reset,
            seed,
            batch,
            diverge,
        },
    )
    .await?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
