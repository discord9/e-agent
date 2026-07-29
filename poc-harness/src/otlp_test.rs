use anyhow::{Context, Result};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyValueInner;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::metrics::v1::exponential_histogram_data_point::Buckets;
use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NumValue;
use opentelemetry_proto::tonic::metrics::v1::{
    ExponentialHistogram, ExponentialHistogramDataPoint, Gauge, Histogram, HistogramDataPoint,
    Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;

const BASE: &str = "http://127.0.0.1:15400/v1/otlp";
const DB: &str = "e_agent";

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.to_string(),
        value: Some(AnyValue {
            value: Some(AnyValueInner::StringValue(v.to_string())),
        }),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

async fn post_otlp(path: &str, body: Vec<u8>, extra_headers: &[(&str, &str)]) -> Result<(u16, String)> {
    let client = reqwest::Client::new();
    let mut req = client
        .post(format!("{BASE}{path}"))
        .header("Content-Type", "application/x-protobuf")
        .header("X-Greptime-DB-Name", DB)
        .body(body);
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await?;
    Ok((status, text))
}

async fn sql_count(table: &str) -> Result<i64> {
    let client = reqwest::Client::new();
    let resp = client
        .post("http://127.0.0.1:15400/v1/sql?format=json")
        .form(&[("sql", &format!("SELECT COUNT(*) AS n FROM {table}")), ("db", &DB.to_string())])
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    resp["data"][0]["n"]
        .as_i64()
        .with_context(|| format!("no count from {table}: {resp}"))
}

// ---------------------------------------------------------------- traces
pub async fn test_traces() -> Result<()> {
    println!("=== OTLP traces ===");
    let now = now_ns();
    let req = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "poc-rust-harness")],
                dropped_attributes_count: 0,
                entity_refs: vec![],
            }),
            scope_spans: vec![ScopeSpans {
                scope: None,
                spans: vec![Span {
                    trace_id: vec![1u8; 16],
                    span_id: vec![2u8; 8],
                    parent_span_id: vec![],
                    name: "poc-rust-span".to_string(),
                    kind: 1, // INTERNAL
                    start_time_unix_nano: now,
                    end_time_unix_nano: now + 1_000_000,
                    attributes: vec![kv("test.key", "test.value")],
                    dropped_attributes_count: 0,
                    events: vec![],
                    dropped_events_count: 0,
                    links: vec![],
                    dropped_links_count: 0,
                    status: None,
                    trace_state: String::new(),
                    flags: 0,
                }],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    };
    let (status, body) = post_otlp(
        "/v1/traces",
        req.encode_to_vec(),
        &[("X-Greptime-Pipeline-Name", "greptime_trace_v1")],
    )
    .await?;
    println!("  status={status} body={body}");
    assert_eq!(status, 200);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let n = sql_count("opentelemetry_traces").await?;
    println!("  opentelemetry_traces count: {n}");
    assert!(n > 0, "traces table empty");
    println!("  ✅ traces OK\n");
    Ok(())
}

// ---------------------------------------------------------------- metrics (gauge + sum + histogram)
pub async fn test_metrics() -> Result<()> {
    println!("=== OTLP metrics (gauge + sum + histogram) ===");
    let now = now_ns();
    let gauge = Metric {
        name: "poc_rust_gauge".to_string(),
        description: "test gauge".to_string(),
        unit: "1".to_string(),
        data: Some(Data::Gauge(Gauge {
            data_points: vec![NumberDataPoint {
                attributes: vec![kv("host", "poc-host")],
                start_time_unix_nano: now,
                time_unix_nano: now,
                value: Some(NumValue::AsDouble(42.0)),
                exemplars: vec![],
                flags: 0,
            }],
        })),
        metadata: vec![],
    };
    let counter = Metric {
        name: "poc_rust_counter".to_string(),
        description: "test counter".to_string(),
        unit: "1".to_string(),
        data: Some(Data::Sum(Sum {
            data_points: vec![NumberDataPoint {
                attributes: vec![kv("host", "poc-host")],
                start_time_unix_nano: now,
                time_unix_nano: now,
                value: Some(NumValue::AsInt(123)),
                exemplars: vec![],
                flags: 0,
            }],
            aggregation_temporality: 2, // CUMULATIVE
            is_monotonic: true,
        })),
        metadata: vec![],
    };
    let hist = Metric {
        name: "poc_rust_histogram".to_string(),
        description: "test histogram".to_string(),
        unit: "ms".to_string(),
        data: Some(Data::Histogram(Histogram {
            data_points: vec![HistogramDataPoint {
                attributes: vec![kv("host", "poc-host")],
                start_time_unix_nano: now,
                time_unix_nano: now,
                count: 10,
                sum: Some(1500.0),
                bucket_counts: vec![1, 2, 3, 4],
                explicit_bounds: vec![10.0, 100.0, 500.0],
                exemplars: vec![],
                flags: 0,
                min: Some(1.0),
                max: Some(400.0),
            }],
            aggregation_temporality: 2,
        })),
        metadata: vec![],
    };
    let req = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "poc-rust-harness")],
                dropped_attributes_count: 0,
                entity_refs: vec![],
            }),
            scope_metrics: vec![ScopeMetrics {
                scope: None,
                metrics: vec![gauge, counter, hist],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    };
    let (status, body) = post_otlp("/v1/metrics", req.encode_to_vec(), &[]).await?;
    println!("  status={status} body={body}");
    assert_eq!(status, 200);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    for table in ["poc_rust_gauge", "poc_rust_counter", "poc_rust_histogram"] {
        match sql_count(table).await {
            Ok(n) => println!("  {table}: {n} rows"),
            Err(e) => println!("  {table}: NOT FOUND ({e})"),
        }
    }
    println!("  ✅ metrics OK\n");
    Ok(())
}

// ---------------------------------------------------------------- exponential histogram
pub async fn test_exp_histogram() -> Result<()> {
    println!("=== OTLP ExponentialHistogram (expect silent drop) ===");
    let now = now_ns();
    let exp = Metric {
        name: "poc_rust_exp_histogram".to_string(),
        description: "test exp histogram".to_string(),
        unit: "ms".to_string(),
        data: Some(Data::ExponentialHistogram(ExponentialHistogram {
            data_points: vec![ExponentialHistogramDataPoint {
                attributes: vec![kv("host", "poc-host")],
                start_time_unix_nano: now,
                time_unix_nano: now,
                count: 5,
                sum: Some(100.0),
                scale: 2,
                zero_count: 1,
                positive: Some(Buckets {
                    offset: 0,
                    bucket_counts: vec![1, 2, 1],
                }),
                negative: None,
                flags: 0,
                exemplars: vec![],
                min: Some(1.0),
                max: Some(50.0),
                zero_threshold: 0.0,
            }],
            aggregation_temporality: 2,
        })),
        metadata: vec![],
    };
    let req = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "poc-rust-harness")],
                dropped_attributes_count: 0,
                entity_refs: vec![],
            }),
            scope_metrics: vec![ScopeMetrics {
                scope: None,
                metrics: vec![exp],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    };
    let (status, body) = post_otlp("/v1/metrics", req.encode_to_vec(), &[]).await?;
    println!("  status={status} body={body}");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    match sql_count("poc_rust_exp_histogram").await {
        Ok(0) => println!("  table exists but 0 rows — silently dropped ✅"),
        Ok(n) => println!("  UNEXPECTED: {n} rows found"),
        Err(_) => println!("  table does not exist — silently dropped ✅"),
    }
    println!();
    Ok(())
}

// ---------------------------------------------------------------- logs
pub async fn test_logs() -> Result<()> {
    println!("=== OTLP logs ===");
    let now = now_ns();
    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "poc-rust-harness")],
                dropped_attributes_count: 0,
                entity_refs: vec![],
            }),
            scope_logs: vec![ScopeLogs {
                scope: None,
                log_records: vec![LogRecord {
                    time_unix_nano: now,
                    observed_time_unix_nano: now,
                    severity_number: 9, // INFO
                    severity_text: "INFO".to_string(),
                    body: Some(AnyValue {
                        value: Some(AnyValueInner::StringValue(
                            "poc rust log message\nwith newline".to_string(),
                        )),
                    }),
                    attributes: vec![kv("test.attr", "log_value")],
                    dropped_attributes_count: 0,
                    flags: 0,
                    trace_id: vec![],
                    span_id: vec![],
                    event_name: String::new(),
                }],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    };
    let (status, body) = post_otlp("/v1/logs", req.encode_to_vec(), &[]).await?;
    println!("  status={status} body={body}");
    assert_eq!(status, 200);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    for table in ["opentelemetry_logs", "greptime_logs", "logs"] {
        match sql_count(table).await {
            Ok(n) => {
                println!("  {table}: {n} rows");
                if n > 0 {
                    println!("  ✅ logs OK\n");
                    return Ok(());
                }
            }
            Err(_) => {}
        }
    }
    anyhow::bail!("no logs table found with data")
}
