use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::DEBUG)
        .init();
    let config = BrowserConfig::builder()
        .window_size(1200, 800)
        .build()
        .unwrap();
    let (browser, mut handler) = Browser::launch(config).await.unwrap();
    let h = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = browser.new_page("about:blank").await.unwrap();
    println!("page opened");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    drop(page);
    drop(browser);
    h.abort();
}
