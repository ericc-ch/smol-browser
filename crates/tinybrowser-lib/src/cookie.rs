use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tinybrowser_net::{cookies::file as cookie_file, Cookie as NetCookie, CookieJar, CookieQuery, ParseAction, ParseOpts};

/// A cookie as exposed to the Rust API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
}

impl Cookie {
    /// Create a cookie from name=value pair with defaults.
    pub fn new(
        name: impl Into<String>,
        value: impl Into<String>,
        domain: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
            path: "/".into(),
            secure: false,
            http_only: false,
        }
    }
}

/// Cookie management for a browser session.
pub struct CookieStore {
    jar: Arc<CookieJar>,
}

impl CookieStore {
    pub(crate) fn new(jar: Arc<CookieJar>) -> Self {
        Self { jar }
    }

    /// Set a cookie via Set-Cookie header string.
    ///
    /// Example: `store.set("session=abc123; Domain=example.com; Path=/; HttpOnly")?;`
    pub fn set(&self, set_cookie_str: &str, url: &str) -> Result<(), crate::error::Error> {
        let parsed = url::Url::parse(url).map_err(|e| crate::error::Error::Internal(e.into()))?;
        match NetCookie::parse(set_cookie_str, &parsed, ParseOpts { allow_http_only: true }) {
            Ok(ParseAction::Store(c)) => { let _ = self.jar.store(c); }
            Ok(ParseAction::Remove(k)) => { let _ = self.jar.remove(&k); }
            Err(e) => return Err(crate::error::Error::Internal(e.into())),
        }
        Ok(())
    }

    /// Get all cookies as a serializable list.
    pub fn get_all(&self) -> Vec<Cookie> {
        self.jar
            .all()
            .into_iter()
            .map(|c| Cookie {
                name: c.name.0,
                value: c.value,
                domain: c.domain.0,
                path: c.path.0,
                secure: c.secure,
                http_only: c.http_only,
            })
            .collect()
    }

    /// Get cookies for a specific URL.
    pub fn get_for_url(&self, url: &str) -> Result<Vec<Cookie>, crate::error::Error> {
        let parsed = url::Url::parse(url).map_err(|e| crate::error::Error::Internal(e.into()))?;
        let header = self.jar.cookies_for(&parsed, CookieQuery { include_http_only: true });
        Ok(header
            .split("; ")
            .filter(|s| !s.is_empty())
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                Some(Cookie {
                    name: parts.next()?.to_string(),
                    value: parts.next().unwrap_or("").to_string(),
                    domain: parsed.host_str()?.to_string(),
                    path: "/".into(),
                    secure: false,
                    http_only: false,
                })
            })
            .collect())
    }

    /// Save cookies to a file (JSON format).
    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), crate::error::Error> {
        cookie_file::save(&self.jar, path)
            .map_err(|e| crate::error::Error::Internal(e.into()))
    }

    /// Load cookies from a file.
    pub fn load_from_file(&self, path: &std::path::Path) -> Result<usize, crate::error::Error> {
        cookie_file::load(&self.jar, path)
            .map_err(|e| crate::error::Error::Internal(e.into()))
    }
}
