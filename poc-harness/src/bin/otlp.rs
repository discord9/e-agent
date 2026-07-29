#[path = "../otlp_test.rs"]
mod otlp_test;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    otlp_test::test_traces().await?;
    otlp_test::test_metrics().await?;
    otlp_test::test_exp_histogram().await?;
    otlp_test::test_logs().await?;
    println!("=== ALL OTLP TESTS DONE ===");
    Ok(())
}
