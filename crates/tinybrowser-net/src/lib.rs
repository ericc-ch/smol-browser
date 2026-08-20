pub mod blocklist;
mod cache;
pub mod callbacks;
pub mod client;
pub mod cookies;
mod cors;
pub mod encoding;
pub mod interceptor;
pub mod robots;
pub mod ssrf;
pub mod types;

pub use blocklist::is_blocked as is_tracker_blocked;
pub use callbacks::{CallbackRegistry, RequestCallback, ResponseCallback};
pub use client::{
    HttpClient, STEALTH_NAVIGATOR_PLATFORM, STEALTH_UA_PLATFORM, STEALTH_UA_PLATFORM_VERSION,
    STEALTH_USER_AGENT,
};
pub use cookies::{
    default_cookie_path, Cookie, CookieInfo, CookieJar, CookieKey, CookieName, CookiePath,
    CookieQuery, Domain, ParseAction, ParseError, ParseOpts, SameSite, UnixSecs,
};
pub use encoding::{
    decode_html, decode_text, url_encode_query, DecodedHtml, TextDecoder, TextDecoderOptions,
};
pub use robots::RobotsCache;
pub use ssrf::{
    env_allows_private_network, is_forbidden_ip, validate_url, PrivateNetworkPolicy,
};
pub use types::{
    NetError, RequestCredentials, RequestInfo, RequestMode, ResourceRequest, ResourceType, Response,
};
