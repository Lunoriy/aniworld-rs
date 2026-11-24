use anyhow::Result;

#[tokio::main]
async fn main() {
    if let Err(err) = aniworld_rs::cli::run().await {
        // `{:#}` prints the whole anyhow chain, one context per line.
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

#[allow(dead_code)]
fn _assert_result_is_send(_: Result<()>) {}
