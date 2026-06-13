use crate::config::{
    API_BASE_URL, API_VERSION, ENDPOINT_AUTH_CSRF, ENDPOINT_AUTH_SESSION,
    ENDPOINT_DELETE_THREAD, ENDPOINT_SSE_ASK, SESSION_COOKIE,
};
use crate::error::{Error, Result};
use crate::sse::{SseEvent, SseStream, WebResult};
use crate::upload::{self, UploadFile};
use futures_util::{Stream, StreamExt};
use rquest::header::{HeaderValue, ORIGIN, REFERER};
use rquest::{Client as HttpClient, Proxy, cookie::Jar};
use rquest_util::Emulation;
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_TIMEOUT: Duration = Duration::from_secs(360);

const PERPLEXITY_ORIGIN: HeaderValue = HeaderValue::from_static(API_BASE_URL);
const PERPLEXITY_REFERER: HeaderValue = HeaderValue::from_static("https://www.perplexity.ai/");

/// Perplexity API client with TLS fingerprint emulation.
pub struct Client {
    http: HttpClient,
    timeout: Duration,
}

#[derive(Serialize)]
struct AskPayload {
    query_str: String,
    params: AskParams,
}

#[derive(Serialize)]
struct AskParams {
    attachments: Vec<String>,
    frontend_context_uuid: String,
    frontend_uuid: String,
    is_incognito: bool,
    language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_backend_uuid: Option<String>,
    mode: String,
    model_preference: String,
    source: String,
    sources: Vec<String>,
    version: String,
}

/// Result of a completed query.
pub struct QueryResult {
    /// Collected answer text (empty for search-only mode).
    #[allow(dead_code)]
    pub answer: String,
    pub web_results: Vec<WebResult>,
    pub backend_uuid: Option<String>,
    pub read_write_token: Option<String>,
}

impl Client {
    /// Build a new client with the given session token and optional proxy.
    pub async fn new(session_token: &str, proxy_url: Option<&str>) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let url = API_BASE_URL
            .parse::<url::Url>()
            .map_err(|_| Error::InvalidBaseUrl)?;

        // Set session cookie
        let cookie = format!(
            "{SESSION_COOKIE}={session_token}; Domain=www.perplexity.ai; Path=/; Secure"
        );
        jar.add_cookie_str(&cookie, &url);

        let mut builder = HttpClient::builder()
            .emulation(Emulation::Chrome136)
            .cookie_provider(jar)
            .timeout(DEFAULT_TIMEOUT);

        if let Some(proxy) = proxy_url {
            let proxy = Proxy::all(proxy).map_err(|e| Error::InvalidProxy(e.to_string()))?;
            builder = builder.proxy(proxy);
        }

        let http = builder.build().map_err(Error::HttpClientInit)?;

        // Warm up: hit session endpoint to establish cookies
        let session_fut = http
            .get(format!("{API_BASE_URL}{ENDPOINT_AUTH_SESSION}"))
            .send();
        tokio::time::timeout(DEFAULT_TIMEOUT, session_fut)
            .await
            .map_err(|_| Error::Timeout(DEFAULT_TIMEOUT))?
            .map_err(Error::SessionWarmup)?;

        Ok(Self {
            http,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Fetch CSRF token from /api/auth/csrf.
    pub async fn fetch_csrf(&self) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct CsrfResponse {
            #[serde(rename = "csrfToken")]
            csrf_token: String,
        }

        let resp = self
            .http
            .get(format!("{API_BASE_URL}{ENDPOINT_AUTH_CSRF}"))
            .send()
            .await
            .map_err(Error::CsrfFetch)?;

        let csrf: CsrfResponse = resp.json().await.map_err(Error::CsrfFetch)?;
        Ok(csrf.csrf_token)
    }

    /// Execute a streaming query. Returns a stream of SSE events.
    pub async fn query_stream(
        &self,
        query: &str,
        mode: &str,
        model_preference: &str,
        incognito: bool,
        files: &[&Path],
        last_backend_uuid: Option<&str>,
        language: &str,
        sources: &[&str],
    ) -> Result<impl Stream<Item = Result<SseEvent>>> {
        // Upload files if any
        let mut attachments = Vec::new();
        if !files.is_empty() {
            let upload_files: Vec<UploadFile> = files
                .iter()
                .map(|p| UploadFile::from_path(p))
                .collect::<std::result::Result<_, _>>()?;
            let urls = upload::upload_files(&self.http, &upload_files, self.timeout).await?;
            attachments = urls;
        }

        let sources_vec: Vec<String> = sources.iter().map(|s| s.to_string()).collect();

        let payload = AskPayload {
            query_str: query.to_string(),
            params: AskParams {
                attachments,
                frontend_context_uuid: Uuid::new_v4().to_string(),
                frontend_uuid: Uuid::new_v4().to_string(),
                is_incognito: incognito,
                language: language.to_string(),
                last_backend_uuid: last_backend_uuid.map(String::from),
                mode: mode.to_string(),
                model_preference: model_preference.to_string(),
                source: "default".to_string(),
                sources: sources_vec,
                version: API_VERSION.to_string(),
            },
        };

        let request_fut = self
            .http
            .post(format!("{API_BASE_URL}{ENDPOINT_SSE_ASK}"))
            .header(ORIGIN, PERPLEXITY_ORIGIN)
            .header(REFERER, PERPLEXITY_REFERER)
            .json(&payload)
            .send();

        let response = tokio::time::timeout(STREAM_TIMEOUT, request_fut)
            .await
            .map_err(|_| Error::Timeout(STREAM_TIMEOUT))?
            .map_err(Error::SearchRequest)?
            .error_for_status()
            .map_err(|e| Error::Server {
                status: e.status().map(|s| s.as_u16()).unwrap_or(0),
                message: e.to_string(),
            })?;

        let requested_model = if model_preference != "turbo" {
            Some(model_preference.to_string())
        } else {
            None
        };

        Ok(SseStream::new(response.bytes_stream(), requested_model))
    }

    /// Execute a query and collect the full response.
    pub async fn query(
        &self,
        query: &str,
        mode: &str,
        model_preference: &str,
        incognito: bool,
        files: &[&Path],
        last_backend_uuid: Option<&str>,
        language: &str,
        sources: &[&str],
    ) -> Result<QueryResult> {
        let stream = self
            .query_stream(
                query,
                mode,
                model_preference,
                incognito,
                files,
                last_backend_uuid,
                language,
                sources,
            )
            .await?;

        let mut answer = String::new();
        let mut web_results = Vec::new();
        let mut backend_uuid = None;
        let mut read_write_token = None;

        let mut stream = Box::pin(stream);
        while let Some(event) = stream.next().await {
            match event? {
                SseEvent::Delta { text } => {
                    answer.push_str(&text);
                }
                SseEvent::Answer { text, web_results: wr, backend_uuid: bu, read_write_token: rwt } => {
                    answer = text;
                    if !wr.is_empty() {
                        web_results = wr;
                    }
                    if bu.is_some() {
                        backend_uuid = bu;
                    }
                    if rwt.is_some() {
                        read_write_token = rwt;
                    }
                }
                SseEvent::WebResults { items } => {
                    web_results = items;
                }
                SseEvent::Done {
                    backend_uuid: uuid,
                    read_write_token: rwt,
                } => {
                    if uuid.is_some() {
                        backend_uuid = uuid;
                    }
                    if rwt.is_some() {
                        read_write_token = rwt;
                    }
                }
                SseEvent::SearchStatus { .. } | SseEvent::Metadata { .. } => {}
                SseEvent::ModelDowngrade => {
                    eprintln!("\n⚠️  Model downgrade detected — session token may have expired");
                }
                SseEvent::Error { message } => {
                    return Err(Error::Server {
                        status: 0,
                        message,
                    });
                }
            }
        }

        Ok(QueryResult {
            answer,
            web_results,
            backend_uuid,
            read_write_token,
        })
    }

    /// Execute a query with streaming output to stdout. Returns session info for multi-turn.
    pub async fn query_live(
        &self,
        query: &str,
        mode: &str,
        model_preference: &str,
        incognito: bool,
        files: &[&Path],
        last_backend_uuid: Option<&str>,
        language: &str,
        sources: &[&str],
    ) -> Result<QueryResult> {
        let stream = self
            .query_stream(
                query,
                mode,
                model_preference,
                incognito,
                files,
                last_backend_uuid,
                language,
                sources,
            )
            .await?;

        let mut answer = String::new();
        let mut web_results = Vec::new();
        let mut backend_uuid = None;
        let mut read_write_token = None;
        let mut streamed = false; // whether any delta text was printed
        let mut showed_progress = false;
        let stderr_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());

        let mut stream = Box::pin(stream);
        while let Some(event) = stream.next().await {
            match event? {
                SseEvent::Delta { text } => {
                    if !text.is_empty() {
                        if showed_progress {
                            if stderr_tty {
                                eprint!("\r\u{1b}[K"); // clear progress line
                            } else {
                                eprintln!();
                            }
                            showed_progress = false;
                        }
                        print!("{text}");
                        answer.push_str(&text);
                        streamed = true;
                    }
                }
                SseEvent::Answer { text, web_results: wr, backend_uuid: bu, read_write_token: rwt } => {
                    // Only print if we didn't already stream via deltas
                    if !streamed && !text.is_empty() {
                        if showed_progress {
                            if stderr_tty {
                                eprint!("\r\u{1b}[K");
                            } else {
                                eprintln!();
                            }
                            showed_progress = false;
                        }
                        print!("{text}");
                    }
                    if !text.is_empty() {
                        answer = text;
                    }
                    if !wr.is_empty() {
                        web_results = wr;
                    }
                    if bu.is_some() {
                        backend_uuid = bu;
                    }
                    if rwt.is_some() {
                        read_write_token = rwt;
                    }
                }
                SseEvent::WebResults { items } => {
                    web_results = items;
                }
                SseEvent::Done {
                    backend_uuid: uuid,
                    read_write_token: rwt,
                } => {
                    if uuid.is_some() {
                        backend_uuid = uuid;
                    }
                    if rwt.is_some() {
                        read_write_token = rwt;
                    }
                }
                SseEvent::SearchStatus { progress } => {
                    // Show search progress on stderr (only in interactive terminal)
                    if stderr_tty {
                        eprint!("\r🔍 {progress}          ");
                        showed_progress = true;
                    }
                }
                SseEvent::Metadata { thread_title, related_queries, display_model } => {
                    if let Some(model) = display_model {
                        eprintln!("\n📋 Model: {model}");
                    }
                    if let Some(title) = thread_title {
                        eprintln!("📝 Thread: {title}");
                    }
                    if !related_queries.is_empty() {
                        eprintln!("💡 Related:");
                        for q in &related_queries {
                            eprintln!("   • {q}");
                        }
                    }
                }
                SseEvent::ModelDowngrade => {
                    eprintln!("\n⚠️  Model downgrade detected — session token may have expired");
                }
                SseEvent::Error { message } => {
                    eprintln!("\n❌ {message}");
                    return Err(Error::Server {
                        status: 0,
                        message,
                    });
                }
            }
        }

        println!(); // final newline
        Ok(QueryResult {
            answer,
            web_results,
            backend_uuid,
            read_write_token,
        })
    }

    /// Delete a thread by entry_uuid.
    pub async fn delete_thread(
        &self,
        entry_uuid: &str,
        read_write_token: Option<&str>,
    ) -> Result<()> {
        let body = serde_json::json!({
            "entry_uuid": entry_uuid,
            "read_write_token": read_write_token.unwrap_or(""),
        });

        let url = format!(
            "{API_BASE_URL}{ENDPOINT_DELETE_THREAD}?version={API_VERSION}&source=default"
        );

        let resp = self
            .http
            .delete(&url)
            .header(ORIGIN, PERPLEXITY_ORIGIN)
            .header(REFERER, PERPLEXITY_REFERER)
            .header("x-app-apiclient", "default")
            .header("x-app-apiversion", API_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::ThreadDelete(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::ThreadDelete(format!(
                "HTTP {status}: {text}"
            )));
        }

        Ok(())
    }
}
