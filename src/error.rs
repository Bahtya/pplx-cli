use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("HTTP client initialization failed: {0}")]
    HttpClientInit(#[source] rquest::Error),

    #[error("Session warmup failed: {0}")]
    SessionWarmup(#[source] rquest::Error),

    #[error("CSRF fetch failed: {0}")]
    CsrfFetch(#[source] rquest::Error),

    #[error("Search request failed: {0}")]
    SearchRequest(#[source] rquest::Error),

    #[error("Upload request failed: {0}")]
    UploadRequest(#[source] rquest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Request timed out after {0:?}")]
    Timeout(Duration),

    #[error("File uploads require PERPLEXITY_SESSION_TOKEN")]
    #[allow(dead_code)]
    FileUploadRequiresAuth,

    #[error("Failed to get upload URL: {0}")]
    UploadUrlFailed(#[source] rquest::Error),

    #[error("S3 upload failed: {0}")]
    S3UploadFailed(#[source] rquest::Error),

    #[error("Missing file entry in batch upload response")]
    MissingUploadResponse,

    #[error("Attachment processing failed: {0}")]
    AttachmentProcessing(#[source] rquest::Error),

    #[error("Invalid MIME type: {0}")]
    InvalidMimeType(String),

    #[error("Invalid UTF-8 in SSE stream")]
    #[allow(dead_code)]
    InvalidUtf8,

    #[error("Server error: {status} - {message}")]
    Server { status: u16, message: String },

    #[error("Stream ended unexpectedly")]
    #[allow(dead_code)]
    UnexpectedEndOfStream,

    #[error("Invalid base URL")]
    InvalidBaseUrl,

    #[error("Invalid proxy URL: {0}")]
    InvalidProxy(String),

    #[error("PERPLEXITY_SESSION_TOKEN environment variable not set")]
    MissingSessionToken,

    #[error("Thread deletion failed: {0}")]
    ThreadDelete(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
