use anyhow::{Context, Result};
use greptimedb_ingester::api::v1::*;
use greptimedb_ingester::client::Client;
use greptimedb_ingester::database::Database;
use greptimedb_ingester::helpers::schema::*;
use greptimedb_ingester::helpers::values::*;

const PG_CONN: &str = "host=127.0.0.1 port=15403 dbname=e_agent";
const GRPC_ENDPOINT: &str = "127.0.0.1:15401";
const HTTP_SQL: &str = "http://127.0.0.1:15400/v1/sql";
const DB: &str = "e_agent";

fn test_payloads() -> Vec<String> {
    vec![
        "plain text".to_string(),
        "line one\nline two\nline three".to_string(),
        "tab\there".to_string(),
        "quote'single".to_string(),
        "你好世界👋\n多行混合".to_string(),
        r#"'); DROP TABLE session_entries; --"#.to_string(),
        "carriage\r\nreturn".to_string(),
        "backslash \\ and \\\\ more".to_string(),
        "A".repeat(1_000_000),
    ]
}

async fn http_sql_query(sql: &str) -> Result<Vec<serde_json::Value>> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{HTTP_SQL}?format=json"))
        .form(&[("sql", sql), ("db", DB)])
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp["data"].as_array().cloned().unwrap_or_default())
}

async fn fetch_payload(session_id: &str, seq: i64) -> Result<String> {
    let rows = http_sql_query(&format!(
        "SELECT payload FROM session_entries WHERE session_id='{session_id}' AND seq={seq}"
    ))
    .await?;
    rows.first()
        .and_then(|r| r["payload"].as_str())
        .map(String::from)
        .with_context(|| format!("no row for {session_id}/{seq}"))
}

// ---------------------------------------------------------------- tokio-postgres
async fn test_tokio_postgres() -> Result<bool> {
    println!("=== tokio-postgres round-trip test ===");
    let (client, conn) = tokio_postgres::connect(PG_CONN, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("pg conn error: {e}");
        }
    });

    let payloads = test_payloads();
    let mut all_ok = true;

    for (i, p) in payloads.iter().enumerate() {
        let seq = i as i64;
        // event_time is TIMESTAMP(9): pass as RFC-3339 string literal, server parses it
        let ts_str = format!("2026-07-29 12:00:00.{:09}", seq);
        let sql = format!(
            "INSERT INTO session_entries(session_id, seq, event_time, entry_kind, payload, schema_version, agent_role, is_error) VALUES($1,$2,'{}','message',$3,1,'main',false)",
            ts_str
        );
        client
            .execute(&sql, &[&"pg-roundtrip", &seq, p])
            .await
            .with_context(|| format!("pg insert seq {seq}"))?;
    }

    for (i, want) in payloads.iter().enumerate() {
        let seq = i as i64;
        let row = client
            .query_one(
                "SELECT payload FROM session_entries WHERE session_id=$1 AND seq=$2",
                &[&"pg-roundtrip", &seq],
            )
            .await?;
        let got: String = row.get(0);
        if &got == want {
            println!("  seq {seq}: OK ({} bytes)", want.len());
        } else {
            all_ok = false;
            println!(
                "  seq {seq}: MISMATCH (got {} bytes, want {} bytes)",
                got.len(),
                want.len()
            );
            let gb = got.as_bytes();
            let wb = want.as_bytes();
            for j in 0..gb.len().min(wb.len()) {
                if gb[j] != wb[j] {
                    println!(
                        "    first diff at byte {j}: got {:02x?} want {:02x?}",
                        &gb[j.saturating_sub(2)..(j + 3).min(gb.len())],
                        &wb[j.saturating_sub(2)..(j + 3).min(wb.len())]
                    );
                    break;
                }
            }
        }
    }
    println!(
        "tokio-postgres: {}\n",
        if all_ok { "ALL OK" } else { "FAILURES" }
    );
    Ok(all_ok)
}

// ---------------------------------------------------------------- gRPC ingester
fn session_schema() -> Vec<ColumnSchema> {
    vec![
        tag("session_id", ColumnDataType::String),
        field("seq", ColumnDataType::Int64),
        timestamp("event_time", ColumnDataType::TimestampNanosecond),
        field("entry_kind", ColumnDataType::String),
        field("payload", ColumnDataType::String),
        field("schema_version", ColumnDataType::Int32),
        field("agent_role", ColumnDataType::String),
        field("is_error", ColumnDataType::Boolean),
    ]
}

async fn test_grpc_ingester() -> Result<bool> {
    println!("=== gRPC ingester round-trip test ===");
    let grpc_client = Client::with_urls(&[GRPC_ENDPOINT]);
    let database = Database::new_with_dbname(DB, grpc_client);

    let payloads = test_payloads();
    let base_ts = 1_785_170_100_000_000_000i64;

    let rows: Vec<Row> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| Row {
            values: vec![
                string_value("grpc-roundtrip".to_string()),
                i64_value(i as i64),
                timestamp_nanosecond_value(base_ts + i as i64),
                string_value("message".to_string()),
                string_value(p.clone()),
                i32_value(1),
                string_value("main".to_string()),
                bool_value(false),
            ],
        })
        .collect();

    let request = RowInsertRequests {
        inserts: vec![RowInsertRequest {
            table_name: "session_entries".to_string(),
            rows: Some(Rows {
                schema: session_schema(),
                rows,
            }),
        }],
    };

    let t0 = std::time::Instant::now();
    let affected = database.insert(request).await.context("grpc insert")?;
    println!(
        "  inserted {affected} rows in {:?} ({} payload bytes total)",
        t0.elapsed(),
        payloads.iter().map(|p| p.len()).sum::<usize>()
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let mut all_ok = true;
    for (i, want) in payloads.iter().enumerate() {
        match fetch_payload("grpc-roundtrip", i as i64).await {
            Ok(got) if &got == want => println!("  seq {i}: OK ({} bytes)", want.len()),
            Ok(got) => {
                all_ok = false;
                println!(
                    "  seq {i}: MISMATCH (got {} bytes, want {})",
                    got.len(),
                    want.len()
                );
            }
            Err(e) => {
                all_ok = false;
                println!("  seq {i}: FETCH FAILED: {e:#}");
            }
        }
    }
    println!(
        "gRPC ingester: {}\n",
        if all_ok { "ALL OK" } else { "FAILURES" }
    );
    Ok(all_ok)
}

// ---------------------------------------------------------------- batch perf
async fn test_grpc_batch() -> Result<()> {
    println!("=== gRPC batch throughput test (1000 rows) ===");
    let grpc_client = Client::with_urls(&[GRPC_ENDPOINT]);
    let database = Database::new_with_dbname(DB, grpc_client);

    let rows: Vec<Row> = (0..1000i64)
        .map(|i| Row {
            values: vec![
                string_value("grpc-batch".to_string()),
                i64_value(i),
                timestamp_nanosecond_value(1_785_170_200_000_000_000i64 + i),
                string_value("message".to_string()),
                string_value(format!("batch message {i} {}", "x".repeat((i % 100) as usize))),
                i32_value(1),
                string_value("main".to_string()),
                bool_value(false),
            ],
        })
        .collect();

    let request = RowInsertRequests {
        inserts: vec![RowInsertRequest {
            table_name: "session_entries".to_string(),
            rows: Some(Rows {
                schema: session_schema(),
                rows,
            }),
        }],
    };

    let t0 = std::time::Instant::now();
    let affected = database.insert(request).await?;
    let elapsed = t0.elapsed();
    println!(
        "  {affected} rows in {:?} ({:.0} rows/s)\n",
        elapsed,
        1000.0 / elapsed.as_secs_f64()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let pg_ok = test_tokio_postgres().await?;
    let grpc_ok = test_grpc_ingester().await?;
    test_grpc_batch().await?;

    println!("=== SUMMARY ===");
    println!(
        "tokio-postgres byte-identical: {}",
        if pg_ok { "YES" } else { "NO" }
    );
    println!(
        "gRPC ingester byte-identical:  {}",
        if grpc_ok { "YES" } else { "NO" }
    );
    Ok(())
}
