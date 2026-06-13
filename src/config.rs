pub const API_BASE_URL: &str = "https://www.perplexity.ai";
pub const API_REFERER: &str = "https://www.perplexity.ai/";
pub const API_VERSION: &str = "2.19";

pub const ENDPOINT_AUTH_SESSION: &str = "/api/auth/session";
pub const ENDPOINT_AUTH_CSRF: &str = "/api/auth/csrf";
pub const ENDPOINT_SSE_ASK: &str = "/rest/sse/perplexity_ask";
pub const ENDPOINT_BATCH_UPLOAD_URL: &str = "/rest/uploads/batch_create_upload_urls";
pub const ENDPOINT_ATTACHMENT_PROCESSING: &str = "/rest/sse/attachment_processing/subscribe";
pub const ENDPOINT_DELETE_THREAD: &str = "/rest/thread/delete_thread_by_entry_uuid";

pub const API_MODE_CONCISE: &str = "concise";
pub const API_MODE_COPILOT: &str = "copilot";

/// Session cookie name used by Perplexity.
pub const SESSION_COOKIE: &str = "__Secure-next-auth.session-token";

/// The free-tier model identifier: the `search` default and the silent-downgrade
/// target detected on COMPLETED events (`display_model == "turbo"`).
pub const TURBO_MODEL: &str = "turbo";

/// Hardcoded model table: (display_name, model_preference, search_mode)
pub const MODELS: &[(&str, &str, &str)] = &[
    ("claude-4.8-opus", "claude48opus", "concise"),
    ("claude-4.8-opus-thinking", "claude48opusthinking", "copilot"),
    ("claude-4.6-sonnet", "claude46sonnet", "concise"),
    ("claude-4.6-sonnet-thinking", "claude46sonnetthinking", "copilot"),
    ("gpt-5.5", "gpt55", "concise"),
    ("gpt-5.5-thinking", "gpt55_thinking", "copilot"),
    ("gemini-3.1-pro", "gemini31pro_high", "copilot"),
    ("gemini-3.1-pro-thinking", "gemini31proprocessing", "copilot"),
    ("kimi", "kimik26instant", "concise"),
    ("kimi-thinking", "kimik26thinking", "copilot"),
];

/// Look up a model by display name. Returns `(model_preference, search_mode)`.
pub fn find_model(name: &str) -> Option<(&'static str, &'static str)> {
    MODELS
        .iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, pref, mode)| (*pref, *mode))
}

/// Default model preference for each CLI mode.
pub mod defaults {
    pub const SEARCH_MODEL: &str = super::TURBO_MODEL;
    pub const SEARCH_MODE: &str = "concise";
    pub const ASK_MODEL: &str = "claude46sonnet";
    pub const ASK_MODE: &str = "concise";
    pub const REASON_MODEL: &str = "claude48opusthinking";
    pub const REASON_MODE: &str = "copilot";
}
