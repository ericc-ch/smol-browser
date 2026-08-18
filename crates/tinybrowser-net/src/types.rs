use std::collections::HashMap;

use url::Url;

#[derive(Debug, Clone)]
pub struct Response {
    pub url: Url,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub redirected_from: Vec<Url>,
}

impl Response {
    /// Decode the body as text, honoring the response charset.
    ///
    /// Uses the HTTP `Content-Type` header's `charset=` parameter, then for
    /// HTML responses falls back to sniffing `<meta charset>` in the first
    /// 1KB, then UTF-8. Mirrors browser behaviour per the HTML5 spec.
    pub fn text(&self) -> String {
        if self.is_html() {
            crate::encoding::decode_html(&self.body, self.content_type()).text
        } else {
            crate::encoding::decode_text(&self.body, self.content_type())
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn is_html(&self) -> bool {
        self.content_type()
            .map(|ct| ct.contains("text/html"))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone)]
pub struct RequestInfo {
    pub url: Url,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub resource_type: ResourceType,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum ResourceType {
    Document,
    Script,
    Stylesheet,
    Image,
    Font,
    Xhr,
    Fetch,
    Other,
}

/// Fetch metadata for a browser-owned request. Navigation keeps its existing
/// profile; render resources use this type so they do not masquerade as HTML
/// documents when they move onto the page's asynchronous transport.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum RequestMode {
    Navigate,
    NoCors,
    Cors,
    SameOrigin,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum RequestCredentials {
    Omit,
    SameOrigin,
    Include,
}

impl RequestMode {
    pub(crate) fn header_value(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::NoCors => "no-cors",
            Self::Cors => "cors",
            Self::SameOrigin => "same-origin",
        }
    }

    pub fn from_fetch_mode(mode: &str) -> Self {
        match mode {
            "no-cors" => Self::NoCors,
            "same-origin" => Self::SameOrigin,
            "navigate" => Self::Navigate,
            _ => Self::Cors,
        }
    }
}

impl RequestCredentials {
    pub fn from_fetch_credentials(value: &str) -> Self {
        match value {
            "omit" => Self::Omit,
            "include" => Self::Include,
            _ => Self::SameOrigin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRequest {
    pub resource_type: ResourceType,
    /// Origin-bearing environment that owns the request. This controls CORS,
    /// credentials, and Sec-Fetch-Site and must remain the document/realm for
    /// every descendant in a module graph.
    pub initiator: Option<Url>,
    /// URL used to derive the Referer header. Usually the same as `initiator`,
    /// but a module dependency is referred by its importing module while its
    /// credentials mode is still relative to the owning document.
    pub referrer: Option<Url>,
    pub mode: RequestMode,
    pub credentials: RequestCredentials,
    /// Hard limit for the decoded response body retained by this request.
    /// Callers can lower it for especially constrained resource consumers.
    pub max_response_bytes: usize,
    pub method: String,
    pub body: Option<Vec<u8>>,
    pub headers: HashMap<String, String>,
}

impl ResourceRequest {
    pub fn navigation() -> Self {
        Self {
            resource_type: ResourceType::Document,
            initiator: None,
            referrer: None,
            mode: RequestMode::Navigate,
            credentials: RequestCredentials::Include,
            max_response_bytes: 64 * 1024 * 1024,
            method: "GET".to_string(),
            body: None,
            headers: HashMap::new(),
        }
    }

    pub fn subresource(resource_type: ResourceType, initiator: &Url) -> Self {
        let mode = match resource_type {
            ResourceType::Font | ResourceType::Xhr | ResourceType::Fetch => RequestMode::Cors,
            ResourceType::Document => RequestMode::Navigate,
            ResourceType::Script
            | ResourceType::Stylesheet
            | ResourceType::Image
            | ResourceType::Other => RequestMode::NoCors,
        };
        let credentials = match resource_type {
            ResourceType::Document
            | ResourceType::Script
            | ResourceType::Stylesheet
            | ResourceType::Image
            | ResourceType::Other => RequestCredentials::Include,
            ResourceType::Font | ResourceType::Xhr | ResourceType::Fetch => {
                RequestCredentials::SameOrigin
            }
        };
        Self {
            resource_type,
            initiator: Some(initiator.clone()),
            referrer: Some(initiator.clone()),
            mode,
            credentials,
            max_response_bytes: match resource_type {
                ResourceType::Stylesheet | ResourceType::Font => 16 * 1024 * 1024,
                ResourceType::Script | ResourceType::Other => 32 * 1024 * 1024,
                ResourceType::Document
                | ResourceType::Image
                | ResourceType::Xhr
                | ResourceType::Fetch => 64 * 1024 * 1024,
            },
            method: "GET".to_string(),
            body: None,
            headers: HashMap::new(),
        }
    }

    /// Fetch profile for JavaScript modules. Unlike classic scripts, module
    /// scripts are CORS-enabled and use `same-origin` credentials by default.
    /// Keep this separate from `subresource(Script, ..)`, whose no-CORS,
    /// include-credentials profile is still correct for classic scripts.
    pub fn module_script(initiator: &Url, referrer: &Url) -> Self {
        Self {
            resource_type: ResourceType::Script,
            initiator: Some(initiator.clone()),
            referrer: Some(referrer.clone()),
            mode: RequestMode::Cors,
            credentials: RequestCredentials::SameOrigin,
            max_response_bytes: 32 * 1024 * 1024,
            method: "GET".to_string(),
            body: None,
            headers: HashMap::new(),
        }
    }

    pub fn scripted_fetch(
        initiator: Option<Url>,
        method: String,
        headers: HashMap<String, String>,
        body: Option<Vec<u8>>,
        mode: RequestMode,
        credentials: RequestCredentials,
    ) -> Self {
        Self {
            resource_type: ResourceType::Fetch,
            initiator,
            referrer: None,
            mode,
            credentials,
            max_response_bytes: 64 * 1024 * 1024,
            method,
            body,
            headers,
        }
    }

    pub fn with_max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = max_response_bytes;
        self
    }

    pub(crate) fn destination(&self) -> &'static str {
        match self.resource_type {
            ResourceType::Document => "document",
            ResourceType::Script => "script",
            ResourceType::Stylesheet => "style",
            ResourceType::Image => "image",
            ResourceType::Font => "font",
            ResourceType::Xhr | ResourceType::Fetch | ResourceType::Other => "empty",
        }
    }

    pub(crate) fn accept(&self) -> &'static str {
        match self.resource_type {
            ResourceType::Document => "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
            ResourceType::Stylesheet => "text/css,*/*;q=0.1",
            ResourceType::Image => "image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            ResourceType::Script
            | ResourceType::Font
            | ResourceType::Xhr
            | ResourceType::Fetch
            | ResourceType::Other => "*/*",
        }
    }

    pub(crate) fn sends_credentials_to(&self, target: &Url) -> bool {
        match self.credentials {
            RequestCredentials::Omit => false,
            RequestCredentials::Include => true,
            RequestCredentials::SameOrigin => self
                .initiator
                .as_ref()
                .is_some_and(|initiator| initiator.origin() == target.origin()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("{0}")]
    Ssrf(String),

    #[error("socks proxies are not supported ({proxy}); use an http:// or https:// proxy")]
    UnsupportedProxy { proxy: String },

    #[error("Too many redirects: {0}")]
    TooManyRedirects(String),

    #[error("Request blocked: {0}")]
    Blocked(String),

    #[error("CORS error: {0}")]
    Cors(String),

    #[error("Response body exceeded {limit} byte limit: {url}")]
    ResponseTooLarge { url: String, limit: usize },
}

pub(crate) fn response_too_large(url: &Url, limit: usize) -> NetError {
    NetError::ResponseTooLarge {
        url: url.to_string(),
        limit,
    }
}

#[cfg(test)]
mod tests {
    use super::{RequestCredentials, RequestMode, ResourceRequest, ResourceType};
    use url::Url;

    #[test]
    fn resource_profiles_use_type_specific_fetch_metadata() {
        let document = Url::parse("https://app.example/page?q=1#fragment").unwrap();
        let image = ResourceRequest::subresource(ResourceType::Image, &document);
        assert_eq!(image.mode, RequestMode::NoCors);
        assert_eq!(image.credentials, RequestCredentials::Include);
        assert_eq!(image.destination(), "image");
        assert!(image.accept().starts_with("image/webp"));

        let stylesheet = ResourceRequest::subresource(ResourceType::Stylesheet, &document);
        assert_eq!(stylesheet.destination(), "style");
        assert_eq!(stylesheet.accept(), "text/css,*/*;q=0.1");

        let font = ResourceRequest::subresource(ResourceType::Font, &document);
        assert_eq!(font.mode, RequestMode::Cors);
        assert_eq!(font.credentials, RequestCredentials::SameOrigin);
        assert_eq!(font.destination(), "font");
        assert_eq!(font.accept(), "*/*");

        assert!(image.sends_credentials_to(&Url::parse("https://cdn.example/image.png").unwrap()));
        assert!(font.sends_credentials_to(&Url::parse("https://app.example/font.woff2").unwrap()));
        assert!(!font.sends_credentials_to(&Url::parse("https://cdn.example/font.woff2").unwrap()));

        let module = ResourceRequest::module_script(&document, &document);
        assert_eq!(module.resource_type, ResourceType::Script);
        assert_eq!(module.mode, RequestMode::Cors);
        assert_eq!(module.credentials, RequestCredentials::SameOrigin);
        assert_eq!(module.destination(), "script");
        assert_eq!(module.accept(), "*/*");
        assert!(module.sends_credentials_to(&Url::parse("https://app.example/chunk.js").unwrap()));
        assert!(!module.sends_credentials_to(&Url::parse("https://cdn.example/chunk.js").unwrap()));
    }

    #[test]
    fn fetch_credentials_gate_cookie_send_per_request_origin() {
        let initiator = Url::parse("https://www.example.com/page").unwrap();
        let same = Url::parse("https://www.example.com/api").unwrap();
        let same_default_port = Url::parse("https://www.example.com:443/api").unwrap();
        let cross = Url::parse("https://api.example.com/data").unwrap();

        let mut omit = ResourceRequest::scripted_fetch(
            Some(initiator.clone()),
            "GET".into(),
            Default::default(),
            None,
            RequestMode::Cors,
            RequestCredentials::from_fetch_credentials("omit"),
        );
        assert!(!omit.sends_credentials_to(&same));
        assert!(!omit.sends_credentials_to(&cross));

        omit.credentials = RequestCredentials::from_fetch_credentials("same-origin");
        assert!(omit.sends_credentials_to(&same));
        assert!(omit.sends_credentials_to(&same_default_port));
        assert!(!omit.sends_credentials_to(&cross));

        omit.credentials = RequestCredentials::from_fetch_credentials("include");
        assert!(omit.sends_credentials_to(&same));
        assert!(omit.sends_credentials_to(&cross));
    }
}
