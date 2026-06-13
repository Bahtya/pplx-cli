use crate::config::{
    API_BASE_URL, API_REFERER, API_VERSION, ENDPOINT_ATTACHMENT_PROCESSING,
    ENDPOINT_BATCH_UPLOAD_URL,
};
use crate::error::{Error, Result};
use rquest::header::{HeaderValue, ORIGIN, REFERER};
use rquest::Client as HttpClient;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

const PERPLEXITY_ORIGIN: HeaderValue = HeaderValue::from_static(API_BASE_URL);
const PERPLEXITY_REFERER: HeaderValue = HeaderValue::from_static(API_REFERER);

#[derive(Serialize)]
struct BatchUploadUrlRequest {
    files: HashMap<String, BatchUploadFileInfo>,
}

#[derive(Serialize)]
struct BatchUploadFileInfo {
    filename: String,
    content_type: String,
    source: String,
    file_size: usize,
    force_image: bool,
    skip_parsing: bool,
    persistent_upload: bool,
}

#[derive(serde::Deserialize)]
struct BatchUploadFileResponse {
    results: HashMap<String, BatchUploadFileResults>,
}

#[derive(serde::Deserialize)]
struct BatchUploadFileResults {
    fields: HashMap<String, String>,
    s3_bucket_url: String,
    #[allow(dead_code)]
    s3_object_url: String,
    file_uuid: String,
}

#[derive(Serialize)]
struct ProcessingSubscribeRequest {
    file_uuids: Vec<String>,
}

/// A file to upload.
pub struct UploadFile {
    pub filename: String,
    pub data: Vec<u8>,
}

impl UploadFile {
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let data = std::fs::read(path)?;
        Ok(Self { filename, data })
    }

    fn content_type(&self) -> String {
        mime_guess::from_path(&self.filename)
            .first_or_octet_stream()
            .to_string()
    }

    fn len(&self) -> usize {
        self.data.len()
    }
}

/// Upload files in batch: presigned URLs → parallel S3 upload → processing.
/// Returns S3 object URLs for each file.
pub async fn upload_files(
    http: &HttpClient,
    files: &[UploadFile],
    timeout: Duration,
) -> Result<Vec<String>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }

    // Assign a client UUID to each file for correlation
    let keyed: Vec<(String, &UploadFile)> = files
        .iter()
        .map(|f| (Uuid::new_v4().to_string(), f))
        .collect();

    // Step 1: get presigned upload URLs
    let batch_resp = request_upload_urls(http, &keyed, timeout).await?;

    // Collect per-file metadata preserving order
    let file_metas: Vec<(FileMeta, &BatchUploadFileResults, &UploadFile)> = keyed
        .iter()
        .map(|(client_uuid, file)| {
            let results = batch_resp
                .results
                .get(client_uuid)
                .ok_or(Error::MissingUploadResponse)?;
            let meta = FileMeta {
                s3_object_url: results.s3_object_url.clone(),
                uuid: results.file_uuid.clone(),
            };
            Ok((meta, results, *file))
        })
        .collect::<Result<Vec<_>>>()?;

    // Step 2: upload to S3 in parallel
    let s3_futures: Vec<_> = file_metas
        .iter()
        .map(|(_, results, file)| upload_to_s3(http, results, file, timeout))
        .collect();
    for res in futures_util::future::join_all(s3_futures).await {
        res?;
    }

    // Step 3: wait for server-side processing
    let file_uuids: Vec<String> = file_metas.iter().map(|(m, _, _)| m.uuid.clone()).collect();
    wait_for_processing(http, &file_uuids, timeout).await?;

    Ok(file_metas.into_iter().map(|(m, _, _)| m.s3_object_url).collect())
}

struct FileMeta {
    s3_object_url: String,
    #[allow(dead_code)]
    uuid: String,
}

async fn request_upload_urls(
    http: &HttpClient,
    keyed: &[(String, &UploadFile)],
    timeout: Duration,
) -> Result<BatchUploadFileResponse> {
    let mut files = HashMap::with_capacity(keyed.len());
    for (client_uuid, file) in keyed {
        files.insert(
            client_uuid.clone(),
            BatchUploadFileInfo {
                filename: file.filename.clone(),
                content_type: file.content_type(),
                source: "default".to_string(),
                file_size: file.len(),
                force_image: false,
                skip_parsing: false,
                persistent_upload: false,
            },
        );
    }

    let full_url = format!(
        "{API_BASE_URL}{ENDPOINT_BATCH_UPLOAD_URL}?version={API_VERSION}&source=default"
    );

    let fut = http
        .post(&full_url)
        .header(ORIGIN, PERPLEXITY_ORIGIN)
        .header(REFERER, PERPLEXITY_REFERER)
        .header("x-app-apiclient", "default")
        .header("x-app-apiversion", API_VERSION)
        .json(&BatchUploadUrlRequest { files })
        .send();

    let resp = tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| Error::Timeout(timeout))?
        .map_err(Error::UploadRequest)?
        .error_for_status()
        .map_err(Error::UploadUrlFailed)?;

    resp.json().await.map_err(Error::UploadRequest)
}

async fn upload_to_s3(
    http: &HttpClient,
    results: &BatchUploadFileResults,
    file: &UploadFile,
    timeout: Duration,
) -> Result<()> {
    let content_type = file.content_type();

    let mut form = rquest::multipart::Form::new();
    for (key, value) in &results.fields {
        form = form.text(key.clone(), value.clone());
    }

    let file_part = rquest::multipart::Part::bytes(file.data.clone())
        .file_name(file.filename.clone())
        .mime_str(&content_type)
        .map_err(|e| Error::InvalidMimeType(e.to_string()))?;
    form = form.part("file", file_part);

    let fut = http.post(&results.s3_bucket_url).multipart(form).send();

    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| Error::Timeout(timeout))?
        .map_err(Error::UploadRequest)?
        .error_for_status()
        .map_err(Error::S3UploadFailed)?;

    Ok(())
}

async fn wait_for_processing(
    http: &HttpClient,
    file_uuids: &[String],
    timeout: Duration,
) -> Result<()> {
    let body = ProcessingSubscribeRequest {
        file_uuids: file_uuids.to_vec(),
    };

    let sse_fut = http
        .post(format!("{API_BASE_URL}{ENDPOINT_ATTACHMENT_PROCESSING}"))
        .header("Accept", "text/event-stream")
        .header(ORIGIN, PERPLEXITY_ORIGIN)
        .header(REFERER, PERPLEXITY_REFERER)
        .header("sec-fetch-dest", "empty")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-site", "same-origin")
        .header(
            "x-perplexity-request-endpoint",
            format!("{API_BASE_URL}{ENDPOINT_ATTACHMENT_PROCESSING}"),
        )
        .json(&body)
        .send();

    let resp = tokio::time::timeout(timeout, sse_fut)
        .await
        .map_err(|_| Error::Timeout(timeout))?
        .map_err(Error::UploadRequest)?
        .error_for_status()
        .map_err(Error::AttachmentProcessing)?;

    tokio::time::timeout(timeout, resp.bytes())
        .await
        .map_err(|_| Error::Timeout(timeout))?
        .map_err(Error::AttachmentProcessing)?;

    Ok(())
}
