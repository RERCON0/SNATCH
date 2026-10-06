//! Size-bounded, cancellable HTTP reads for torrent metadata.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Debug)]
pub(crate) enum HttpError {
    Cancelled,
    Transport(String),
    TooLarge,
}
impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "__cancelled__"),
            Self::Transport(s) => write!(f, "сеть: {s}"),
            Self::TooLarge => write!(f, "ответ слишком большой"),
        }
    }
}

pub(crate) async fn request_bytes(
    request: reqwest::RequestBuilder,
    max: u64,
    cancel: &AtomicBool,
) -> Result<(reqwest::StatusCode, Vec<u8>), HttpError> {
    tokio::select! {
        biased;
        _ = cancelled(cancel) => Err(HttpError::Cancelled),
        result = async {
            let mut response = request.send().await.map_err(|e| HttpError::Transport(e.to_string()))?;
            let status = response.status();
            if response.content_length().is_some_and(|n| n > max) {
                return Err(HttpError::TooLarge);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| HttpError::Transport(e.to_string()))? {
                if chunk.len() as u64 > max - bytes.len() as u64 { return Err(HttpError::TooLarge); }
                bytes.extend_from_slice(&chunk);
            }
            Ok((status, bytes))
        } => result,
    }
}

pub(crate) async fn get_bytes(
    client: &reqwest::Client,
    url: &str,
    max: u64,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let (status, bytes) = request_bytes(client.get(url), max, cancel)
        .await
        .map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP: {status}"));
    }
    Ok(bytes)
}
