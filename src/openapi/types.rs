use std::collections::HashMap;
use std::fmt;

/// Authentication method for OpenAPI requests.
#[derive(Clone)]
pub enum OpenApiAuth {
    /// No authentication.
    None,
    /// Bearer token (Authorization: Bearer <token>).
    Bearer(String),
    /// API key in a custom header.
    ApiKey { header: String, value: String },
}

impl fmt::Debug for OpenApiAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "None"),
            Self::Bearer(_) => write!(f, "Bearer(****)"),
            Self::ApiKey { header, .. } => f
                .debug_struct("ApiKey")
                .field("header", header)
                .field("value", &"****")
                .finish(),
        }
    }
}

impl Default for OpenApiAuth {
    fn default() -> Self {
        Self::None
    }
}

/// Configuration for OpenAPI tool adapters.
#[derive(Debug, Clone)]
pub struct OpenApiConfig {
    /// Override base URL from the spec (e.g., for staging environments).
    pub base_url: Option<String>,
    /// Authentication method.
    pub auth: OpenApiAuth,
    /// Additional headers to include on every request.
    pub custom_headers: HashMap<String, String>,
    /// Maximum response body size in bytes (default: 64KB).
    pub max_response_bytes: usize,
    /// Request timeout in seconds (default: 30).
    pub timeout_secs: u64,
    /// Prefix for tool names (e.g., "github" → "github__listRepos").
    pub name_prefix: Option<String>,
}

impl Default for OpenApiConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            auth: OpenApiAuth::None,
            custom_headers: HashMap::new(),
            max_response_bytes: 64 * 1024,
            timeout_secs: 30,
            name_prefix: None,
        }
    }
}

impl OpenApiConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        self.base_url = Some(url.trim_end_matches('/').to_string());
        self
    }

    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.auth = OpenApiAuth::Bearer(token.into());
        self
    }

    pub fn with_api_key(mut self, header: impl Into<String>, value: impl Into<String>) -> Self {
        self.auth = OpenApiAuth::ApiKey {
            header: header.into(),
            value: value.into(),
        };
        self
    }

    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom_headers.insert(key.into(), value.into());
        self
    }

    pub fn with_max_response_bytes(mut self, max: usize) -> Self {
        self.max_response_bytes = max;
        self
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    pub fn with_name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.name_prefix = Some(prefix.into());
        self
    }
}

/// Controls which operations from the spec become tools.
#[derive(Debug, Clone)]
pub enum OperationFilter {
    /// Include all operations.
    All,
    /// Include only operations with these operation IDs.
    ByOperationId(Vec<String>),
    /// Include only operations tagged with any of these tags.
    ByTag(Vec<String>),
    /// Include only operations whose path starts with this prefix.
    ByPathPrefix(String),
}

impl Default for OperationFilter {
    fn default() -> Self {
        Self::All
    }
}

/// Parsed operation metadata (internal).
#[derive(Debug, Clone)]
pub(crate) struct OperationInfo {
    pub operation_id: String,
    pub method: String,
    pub path: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub parameters_schema: serde_json::Value,
    pub path_params: Vec<String>,
    pub query_params: Vec<String>,
    pub header_params: Vec<String>,
    pub has_body: bool,
}

/// Errors from OpenAPI spec parsing and tool execution.
#[derive(Debug, thiserror::Error)]
pub enum OpenApiError {
    #[error("Failed to parse OpenAPI spec: {0}")]
    ParseError(String),
    #[error("HTTP error: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("No base URL found in spec or config")]
    NoBaseUrl,
    #[error("Invalid spec: {0}")]
    InvalidSpec(String),
}
