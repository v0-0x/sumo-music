//! Shared HTTP client.

use std::sync::OnceLock;
use std::time::Duration;

pub const USER_AGENT: &str = concat!(
    "Sumo/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/v0-0x/sumo-music)"
);

/// Most an image may be (covers, avatars): a bigger answer is no image Sumo wants.
pub const MAX_IMAGE: usize = 16 << 20;

/// The body of `resp`, giving up once it passes `max` bytes (counted after decompression, so
/// a small compressed answer can't unpack into gigabytes).
pub async fn read_capped(mut resp: reqwest::Response, max: usize) -> anyhow::Result<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        anyhow::bail!("the answer is too big");
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            anyhow::bail!("the answer is too big");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// An image from the web, at most [`MAX_IMAGE`] bytes.
pub async fn get_image(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let resp = http.get(url).send().await?.error_for_status()?;
    read_capped(resp, MAX_IMAGE).await
}

/// One client for the whole app: clones share the connection pool and the TLS
/// context (which holds the parsed system CA store).
pub fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .pool_max_idle_per_host(2)
                .build()
                .expect("failed to build HTTP client")
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers every request with `reply` (a raw HTTP response).
    async fn serve(reply: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(reply).await;
                let _ = stream.shutdown().await;
            }
        });
        format!("http://{addr}/")
    }

    async fn fetch(reply: &'static [u8], max: usize) -> anyhow::Result<Vec<u8>> {
        let url = serve(reply).await;
        let resp = reqwest::Client::builder().no_proxy().build()?.get(url).send().await?;
        read_capped(resp, max).await
    }

    #[tokio::test]
    async fn answers_are_capped() {
        // Small enough.
        let ok = fetch(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            5,
        )
        .await;
        assert_eq!(ok.unwrap(), b"hello");
        // Says it's too big.
        let big = fetch(
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello!",
            5,
        )
        .await;
        assert!(big.is_err());
        // Doesn't say how big, and turns out too big.
        let chunked = fetch(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nabcd\r\n4\r\nefgh\r\n0\r\n\r\n",
            5,
        )
        .await;
        assert!(chunked.is_err());
    }
}
