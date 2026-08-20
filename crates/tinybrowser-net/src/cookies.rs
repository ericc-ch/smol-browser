use std::collections::HashMap;
use std::sync::RwLock;
use url::Url;

// ---------------------------------------------------------------------------
// New domain types — Ticket 09
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Domain(pub String);
impl Domain {
    pub fn as_str(&self) -> &str { &self.0 }
}
impl std::fmt::Display for Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct CookieName(pub String);
impl CookieName {
    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct CookiePath(pub String);
impl CookiePath {
    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}
impl SameSite {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "strict" => Self::Strict,
            "none" => Self::None,
            _ => Self::Lax,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}
impl std::fmt::Display for SameSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct UnixSecs(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: CookieName,
    pub value: String,
    pub domain: Domain,
    pub path: CookiePath,
    pub same_site: SameSite,
    pub expires: Option<UnixSecs>,
    pub host_only: bool,
    pub secure: bool,
    pub http_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CookieKey {
    pub name: CookieName,
    pub domain: Domain,
    pub path: Option<CookiePath>,
}

#[derive(Debug, Clone, Copy)]
pub struct ParseOpts {
    pub allow_http_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseAction {
    Store(Cookie),
    Remove(CookieKey),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ParseError {
    #[error("empty cookie string")]
    Empty,
    #[error("missing name=value")]
    MissingNameValue,
    #[error("invalid origin host")]
    InvalidOrigin,
}

impl Cookie {
    pub fn parse(raw: &str, origin: &Url, opts: ParseOpts) -> Result<ParseAction, ParseError> {
        let parts: Vec<&str> = raw.splitn(2, ';').collect();
        let name_value = parts.first().map(|s| s.trim()).unwrap_or("");
        if name_value.is_empty() { return Err(ParseError::Empty); }
        let (name, value) = name_value.split_once('=').ok_or(ParseError::MissingNameValue)?;
        let name = CookieName(name.trim().to_string());
        let value = value.trim().to_string();
        if name.0.is_empty() { return Err(ParseError::MissingNameValue); }
        let request_host = origin.host_str().unwrap_or("").to_lowercase();
        if request_host.is_empty() { return Err(ParseError::InvalidOrigin); }
        let mut domain_attr: Option<String> = None;
        let mut path = CookiePath(default_cookie_path(origin.path()));
        let mut secure = false;
        let mut http_only = false;
        let mut expires: Option<UnixSecs> = None;
        let mut same_site = SameSite::Lax;
        let mut max_age_zero = false;
        if parts.len() > 1 {
            for attr in parts[1].split(';') {
                let attr = attr.trim();
                if let Some((key, val)) = attr.split_once('=') {
                    match key.trim().to_ascii_lowercase().as_str() {
                        "domain" => { domain_attr = Some(val.trim().trim_start_matches('.').to_lowercase()); }
                        "path" => { path = CookiePath(val.trim().to_string()); }
                        "expires" => { if let Ok(ts) = parse_http_date_v2(val.trim()) { expires = Some(UnixSecs(ts)); } }
                        "max-age" => {
                            if let Ok(secs) = val.trim().parse::<i64>() {
                                if secs <= 0 { max_age_zero = true; expires = Some(UnixSecs(0)); } else {
                                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                    expires = Some(UnixSecs(now + secs as u64));
                                }
                            }
                        }
                        "samesite" => { same_site = SameSite::parse(val); }
                        _ => {}
                    }
                } else {
                    match attr.to_ascii_lowercase().as_str() {
                        "secure" => secure = true,
                        "httponly" => { if opts.allow_http_only { http_only = true; } },
                        _ => {}
                    }
                }
            }
        }
        let (domain_str, host_only) = resolve_cookie_domain(&request_host, domain_attr.as_deref()).ok_or(ParseError::InvalidOrigin)?;
        let domain = Domain(domain_str);
        if max_age_zero || matches!(expires, Some(UnixSecs(0))) {
            return Ok(ParseAction::Remove(CookieKey { name, domain, path: Some(path) }));
        }
        if let Some(UnixSecs(ts)) = expires {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
            if ts != 0 && ts < now { return Ok(ParseAction::Remove(CookieKey { name, domain, path: Some(path) })); }
        }
        Ok(ParseAction::Store(Cookie { name, value, domain, path, same_site, expires, host_only, secure, http_only }))
    }
}

impl TryFrom<CookieInfo> for Cookie {
    type Error = ParseError;
    fn try_from(info: CookieInfo) -> Result<Self, Self::Error> {
        if info.name.is_empty() || info.domain.is_empty() { return Err(ParseError::MissingNameValue); }
        let same_site = if info.same_site.is_empty() { SameSite::Lax } else { SameSite::parse(&info.same_site) };
        let expires = info.expires.and_then(|e| if e > 0 { Some(UnixSecs(e as u64)) } else { None });
        Ok(Cookie {
            name: CookieName(info.name),
            value: info.value,
            domain: Domain(info.domain),
            path: CookiePath(if info.path.is_empty() { "/".to_string() } else { info.path }),
            same_site,
            expires,
            host_only: false,
            secure: info.secure,
            http_only: info.http_only,
        })
    }
}

impl From<Cookie> for CookieInfo {
    fn from(c: Cookie) -> Self {
        CookieInfo {
            name: c.name.0,
            value: c.value,
            domain: c.domain.0,
            path: c.path.0,
            secure: c.secure,
            http_only: c.http_only,
            same_site: c.same_site.as_str().to_string(),
            expires: c.expires.map(|UnixSecs(e)| e as i64),
        }
    }
}

fn parse_http_date_v2(s: &str) -> Result<u64, ()> {
    if let Ok(t) = httpdate::parse_http_date(s) {
        return Ok(t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    }
    parse_http_date_legacy(s)
}

fn parse_http_date_legacy(s: &str) -> Result<u64, ()> {
    let months = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let s = s.replace('-', " ");
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 { return Err(()); }
    let day: u64 = parts[1].parse().map_err(|_| ())?;
    let month = months.iter().position(|m| parts[2].to_lowercase().starts_with(m)).ok_or(())? as u64 + 1;
    let year: u64 = parts[3].parse().map_err(|_| ())?;
    let time_parts: Vec<&str> = parts[4].split(':').collect();
    let hour: u64 = time_parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minute: u64 = time_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let second: u64 = time_parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut days_total: u64 = 0;
    for y in 1970..year { days_total += if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 }; }
    let days_in_month = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    for m in 1..month { days_total += days_in_month[m as usize] + u64::from(m == 2 && is_leap); }
    days_total += day - 1;
    Ok(days_total * 86400 + hour * 3600 + minute * 60 + second)
}

pub struct CookieJar {
    cookies: RwLock<HashMap<Domain, HashMap<(CookieName, CookiePath), Cookie>>>,
}

#[derive(Debug, Clone)]
pub struct CookieQuery {
    pub include_http_only: bool,
}

impl CookieJar {
    pub fn new() -> Self {
        CookieJar { cookies: RwLock::new(HashMap::new()) }
    }

    // ---- new deep API ----

    pub fn store(&self, cookie: Cookie) -> Result<(), String> {
        let mut jar = self.cookies.write().map_err(|_| "poisoned lock".to_string())?;
        jar.entry(cookie.domain.clone()).or_default().insert((cookie.name.clone(), cookie.path.clone()), cookie);
        Ok(())
    }

    pub fn remove(&self, key: &CookieKey) -> bool {
        let mut removed = false;
        let Ok(mut jar) = self.cookies.write() else { return false };
        let domains_to_try: Vec<Domain> = if key.domain.0.is_empty() {
            jar.keys().cloned().collect()
        } else {
            vec![
                key.domain.clone(),
                Domain(format!(".{}", key.domain.0.trim_start_matches('.'))),
                Domain(key.domain.0.trim_start_matches('.').to_string()),
            ]
        };
        for d in domains_to_try {
            if let Some(bucket) = jar.get_mut(&d) {
                let before = bucket.len();
                match &key.path {
                    Some(p) => { bucket.retain(|(n, path), _| !(n == &key.name && path == p)); },
                    None => { bucket.retain(|(n, _), _| n != &key.name); },
                }
                if bucket.len() != before { removed = true; }
            }
            // also try raw string key variants for compat with old .example.com storage
            // domain_matches handles leading dot, but map key may be with dot
        }
        // also handle leading-dot variants by scanning all
        if !removed && !key.domain.0.is_empty() {
            let needle = key.domain.0.trim_start_matches('.').to_ascii_lowercase();
            for (dom, bucket) in jar.iter_mut() {
                if dom.0.trim_start_matches('.').eq_ignore_ascii_case(&needle) {
                    let before = bucket.len();
                    match &key.path {
                        Some(p) => bucket.retain(|(n, path), _| !(n == &key.name && path == p)),
                        None => bucket.retain(|(n, _), _| n != &key.name),
                    }
                    if bucket.len() != before { removed = true; }
                }
            }
        }
        removed
    }

    pub fn cookies_for(&self, url: &Url, query: CookieQuery) -> String {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let is_secure = url.scheme() == "https";
        let Ok(jar) = self.cookies.read() else { return String::new() };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let mut matching: Vec<String> = Vec::new();
        for (domain, bucket) in jar.iter() {
            if !domain_matches(host, &domain.0) { continue; }
            for cookie in bucket.values() {
                if cookie.host_only && !host.eq_ignore_ascii_case(&domain.0) { continue; }
                if !query.include_http_only && cookie.http_only { continue; }
                if let Some(UnixSecs(exp)) = cookie.expires { if exp != 0 && exp < now { continue; } }
                if cookie.secure && !is_secure { continue; }
                if !path_matches(path, &cookie.path.0) { continue; }
                matching.push(format!("{}={}", cookie.name.0, cookie.value));
            }
        }
        matching.join("; ")
    }

    pub fn all(&self) -> Vec<Cookie> {
        let Ok(jar) = self.cookies.read() else { return Vec::new() };
        jar.values().flat_map(|b| b.values().cloned()).collect()
    }

    pub fn clear(&self) {
        if let Ok(mut jar) = self.cookies.write() { jar.clear(); }
    }

    // ---- old API shims (delegate to new) — to be deleted after call-site migration ----

    pub fn set_cookie(&self, set_cookie_str: &str, url: &Url) {
        match Cookie::parse(set_cookie_str, url, ParseOpts { allow_http_only: true }) {
            Ok(ParseAction::Store(c)) => { let _ = self.store(c); },
            Ok(ParseAction::Remove(k)) => { let _ = self.remove(&k); },
            Err(_) => {},
        }
    }

    pub fn get_cookie_header(&self, url: &Url) -> String {
        self.cookies_for(url, CookieQuery { include_http_only: true })
    }

    pub fn get_all_cookies(&self) -> Vec<CookieInfo> {
        self.all().into_iter().map(CookieInfo::from).collect()
    }

    pub fn set_cookies_from_cdp(&self, cookies: Vec<CookieInfo>) {
        for info in cookies {
            if let Ok(c) = Cookie::try_from(info) { let _ = self.store(c); }
        }
    }

    pub fn get_js_visible_cookies(&self, url: &Url) -> String {
        self.cookies_for(url, CookieQuery { include_http_only: false })
    }

    pub fn set_cookie_from_js(&self, cookie_str: &str, url: &Url) {
        match Cookie::parse(cookie_str, url, ParseOpts { allow_http_only: false }) {
            Ok(ParseAction::Store(c)) => { let _ = self.store(c); },
            Ok(ParseAction::Remove(k)) => { let _ = self.remove(&k); },
            Err(_) => {},
        }
    }

    pub fn delete_cookie(&self, name: &str, domain: &str) {
        let _ = self.remove(&CookieKey { name: CookieName(name.to_string()), domain: Domain(domain.to_string()), path: None });
    }

    pub fn delete_cookies_filtered(&self, name: &str, domain: &str, path: Option<&str>) {
        let _ = self.remove(&CookieKey { name: CookieName(name.to_string()), domain: Domain(domain.to_string()), path: path.map(|p| CookiePath(p.to_string())) });
    }

    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        crate::cookies::file::save(self, path)
    }

    pub fn load_from_file(&self, path: &std::path::Path) -> Result<usize, std::io::Error> {
        crate::cookies::file::load(self, path)
    }
}

impl Default for CookieJar {
    fn default() -> Self { Self::new() }
}

// File adapter — next to store per decision 05
pub mod file {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Persisted {
        name: String,
        value: String,
        domain: String,
        path: String,
        secure: bool,
        #[serde(rename = "httpOnly")]
        http_only: bool,
        #[serde(default, rename = "sameSite")]
        same_site: String,
        #[serde(default)]
        expires: Option<i64>,
        #[serde(default)]
        host_only: bool,
    }

    pub fn save(store: &CookieJar, path: &Path) -> Result<(), std::io::Error> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let cookies = store.cookies.read().map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock"))?;
        let mut all: Vec<Persisted> = Vec::new();
        for c in cookies.values().flat_map(|b| b.values()) {
            if let Some(UnixSecs(exp)) = c.expires { if exp != 0 && exp < now { continue; } }
            all.push(Persisted {
                name: c.name.0.clone(),
                value: c.value.clone(),
                domain: c.domain.0.clone(),
                path: c.path.0.clone(),
                secure: c.secure,
                http_only: c.http_only,
                same_site: c.same_site.as_str().to_string(),
                expires: c.expires.map(|UnixSecs(e)| e as i64),
                host_only: c.host_only,
            });
        }
        let json = serde_json::to_string_pretty(&all).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
        tmp.write_all(json.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    pub fn load(store: &CookieJar, path: &Path) -> Result<usize, std::io::Error> {
        if !path.exists() { return Ok(0); }
        let data = std::fs::read_to_string(path)?;
        let persisted: Vec<Persisted> = serde_json::from_str(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut count = 0;
        for p in persisted {
            let same_site = if p.same_site.is_empty() { SameSite::Lax } else { SameSite::parse(&p.same_site) };
            let expires = p.expires.and_then(|e| if e > 0 { Some(UnixSecs(e as u64)) } else { None });
            let c = Cookie {
                name: CookieName(p.name),
                value: p.value,
                domain: Domain(p.domain),
                path: CookiePath(p.path),
                same_site,
                expires,
                host_only: p.host_only,
                secure: p.secure,
                http_only: p.http_only,
            };
            if let Some(UnixSecs(exp)) = c.expires {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                if exp != 0 && exp < now { continue; }
            }
            let _ = store.store(c);
            count += 1;
        }
        Ok(count)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CookieInfo {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    #[serde(rename = "httpOnly")]
    pub http_only: bool,
    #[serde(default, rename = "sameSite")]
    pub same_site: String,
    #[serde(default)]
    pub expires: Option<i64>,
}

pub fn default_cookie_path(request_path: &str) -> String {
    if !request_path.starts_with('/') { return "/".to_string(); }
    match request_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => request_path[..idx].to_string(),
    }
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path { return true; }
    if !request_path.starts_with(cookie_path) { return false; }
    cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')
}

fn domain_matches(host: &str, domain: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    if host.len() < domain.len() { return false; }
    if host.eq_ignore_ascii_case(domain) { return true; }
    let prefix_len = host.len() - domain.len();
    if prefix_len < 1 { return false; }
    if !host.is_char_boundary(prefix_len) { return false; }
    if host.as_bytes()[prefix_len - 1] != b'.' { return false; }
    host[prefix_len..].eq_ignore_ascii_case(domain)
}

fn resolve_cookie_domain(origin_host: &str, domain_attr: Option<&str>) -> Option<(String, bool)> {
    let origin = origin_host.trim().trim_start_matches('.').to_lowercase();
    if origin.is_empty() { return None; }
    let dom = match domain_attr {
        None => return Some((origin, true)),
        Some(raw) => raw.trim().trim_start_matches('.').to_lowercase(),
    };
    if dom.is_empty() || dom == origin { return Some((origin, true)); }
    if dom.contains('.') && origin.ends_with(&format!(".{dom}")) { Some((dom, false)) } else { Some((origin, true)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn set_cookie(jar: &CookieJar, s: &str, url: &Url) {
        match Cookie::parse(s, url, ParseOpts { allow_http_only: true }).unwrap() {
            ParseAction::Store(c) => { jar.store(c).unwrap(); },
            ParseAction::Remove(k) => { jar.remove(&k); },
        }
    }
    fn set_cookie_js(jar: &CookieJar, s: &str, url: &Url) {
        match Cookie::parse(s, url, ParseOpts { allow_http_only: false }).unwrap() {
            ParseAction::Store(c) => { jar.store(c).unwrap(); },
            ParseAction::Remove(k) => { jar.remove(&k); },
        }
    }
    fn get_header(jar: &CookieJar, url: &Url) -> String { jar.cookies_for(url, CookieQuery { include_http_only: true }) }
    fn get_js(jar: &CookieJar, url: &Url) -> String { jar.cookies_for(url, CookieQuery { include_http_only: false }) }
    fn set_cdp(jar: &CookieJar, infos: Vec<CookieInfo>) { for info in infos { if let Ok(c) = Cookie::try_from(info) { let _ = jar.store(c); } } }
    fn all_cookies(jar: &CookieJar) -> Vec<CookieInfo> { jar.all().into_iter().map(Into::into).collect() }
    fn delete_filtered(jar: &CookieJar, name: &str, domain: &str, path: Option<&str>) { jar.remove(&CookieKey { name: CookieName(name.to_string()), domain: Domain(domain.to_string()), path: path.map(|p| CookiePath(p.to_string())) }); }
    fn save(jar: &CookieJar, path: &std::path::Path) -> Result<(), std::io::Error> { crate::cookies::file::save(jar, path) }
    fn load(jar: &CookieJar, path: &std::path::Path) -> Result<usize, std::io::Error> { crate::cookies::file::load(jar, path) }


    #[test]
    fn test_set_and_get_cookie() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/path").unwrap();
        set_cookie(&jar, "session=abc123; Path=/; Secure; HttpOnly", &url);
        let header = get_header(&jar, &url);
        assert!(header.contains("session=abc123"));
    }

    #[test]
    fn test_cookie_domain_matching() {
        let jar = CookieJar::new();
        let url = Url::parse("https://www.example.com/").unwrap();
        set_cookie(&jar, "token=xyz; Domain=example.com", &url);
        let header = get_header(&jar, &url);
        assert!(header.contains("token=xyz"));
        let sub_url = Url::parse("https://api.example.com/").unwrap();
        let header2 = get_header(&jar, &sub_url);
        assert!(header2.contains("token=xyz"));
        let other_url = Url::parse("https://other.com/").unwrap();
        let header3 = get_header(&jar, &other_url);
        assert!(header3.is_empty());
    }

    #[test]
    fn test_cdp_cookie_with_leading_dot_domain_matches_requests() {
        let jar = CookieJar::new();
        set_cdp(&jar, vec![CookieInfo {
            name: "token".to_string(), value: "xyz".to_string(), domain: ".example.com".to_string(),
            path: "/".to_string(), secure: false, http_only: false, same_site: String::new(), expires: None,
        }]);
        let apex_url = Url::parse("https://example.com/").unwrap();
        assert!(get_header(&jar, &apex_url).contains("token=xyz"));
        let subdomain_url = Url::parse("https://api.example.com/").unwrap();
        assert!(get_header(&jar, &subdomain_url).contains("token=xyz"));
        let other_url = Url::parse("https://other.com/").unwrap();
        assert!(get_header(&jar, &other_url).is_empty());
    }

    #[test]
    fn test_secure_cookie_not_sent_over_http() {
        let jar = CookieJar::new();
        let https_url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "secure_token=secret; Secure", &https_url);
        let http_url = Url::parse("http://example.com/").unwrap();
        assert!(get_header(&jar, &http_url).is_empty());
    }

    #[test]
    fn test_max_age_zero_deletes_cookie() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "session=abc", &url);
        assert!(get_header(&jar, &url).contains("session=abc"));
        set_cookie(&jar, "session=abc; Max-Age=0", &url);
        assert!(get_header(&jar, &url).is_empty());
    }

    #[test]
    fn test_same_name_cookies_with_different_paths_coexist() {
        let jar = CookieJar::new();
        let set_url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "id=1; Path=/a", &set_url);
        set_cookie(&jar, "id=2; Path=/b", &set_url);
        let header_a = get_header(&jar, &Url::parse("https://example.com/a/page").unwrap());
        let header_b = get_header(&jar, &Url::parse("https://example.com/b/page").unwrap());
        assert!(header_a.contains("id=1"));
        assert!(header_b.contains("id=2"));
        assert!(!header_a.contains("id=2"));
        assert!(!header_b.contains("id=1"));
    }

    #[test]
    fn test_same_name_same_path_cookie_is_replaced() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/a/x").unwrap();
        set_cookie(&jar, "id=1; Path=/a", &url);
        set_cookie(&jar, "id=2; Path=/a", &url);
        let header = get_header(&jar, &url);
        assert!(header.contains("id=2"));
        assert!(!header.contains("id=1"));
    }

    #[test]
    fn test_max_age_zero_deletes_only_matching_path() {
        let jar = CookieJar::new();
        let set_url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "id=1; Path=/a", &set_url);
        set_cookie(&jar, "id=2; Path=/b", &set_url);
        set_cookie(&jar, "id=x; Path=/a; Max-Age=0", &set_url);
        let header_a = get_header(&jar, &Url::parse("https://example.com/a/page").unwrap());
        let header_b = get_header(&jar, &Url::parse("https://example.com/b/page").unwrap());
        assert!(header_a.is_empty());
        assert!(header_b.contains("id=2"));
    }

    #[test]
    fn test_max_age_sets_expiry() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "token=xyz; Max-Age=3600", &url);
        assert!(get_header(&jar, &url).contains("token=xyz"));
    }

    #[test]
    fn test_expired_cookie_not_sent() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "old=gone; Expires=Thu, 01 Jan 2020 00:00:00 GMT", &url);
        assert!(get_header(&jar, &url).is_empty());
    }

    #[test]
    fn test_samesite_parsed() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "strict_cookie=val; SameSite=Strict", &url);
        assert!(get_header(&jar, &url).contains("strict_cookie=val"));
        // new enum
        assert_eq!(SameSite::parse("Strict"), SameSite::Strict);
        assert_eq!(SameSite::parse("lax"), SameSite::Lax);
        assert_eq!(SameSite::parse("none"), SameSite::None);
        assert_eq!(SameSite::parse("bogus"), SameSite::Lax);
    }

    #[test]
    fn test_set_cookies_from_cdp_preserves_same_site_and_expires() {
        let jar = CookieJar::new();
        let future_expiry = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + 3600;
        set_cdp(&jar, vec![
            CookieInfo { name: "sid".to_string(), value: "abc".to_string(), domain: "example.com".to_string(), path: "/".to_string(), secure: true, http_only: true, same_site: "Strict".to_string(), expires: Some(future_expiry) },
            CookieInfo { name: "session_only".to_string(), value: "xyz".to_string(), domain: "example.com".to_string(), path: "/".to_string(), secure: false, http_only: false, same_site: String::new(), expires: None },
        ]);
        let cookies = all_cookies(&jar);
        assert_eq!(cookies.len(), 2);
        let sid = cookies.iter().find(|c| c.name == "sid").unwrap();
        assert_eq!(sid.same_site, "Strict");
        assert_eq!(sid.expires, Some(future_expiry));
    }

    #[test]
    fn test_delete_cookies_filtered_path_mismatch_preserves_cookie() {
        let jar = CookieJar::new();
        set_cdp(&jar, vec![CookieInfo { name: "sid".to_string(), value: "v".to_string(), domain: "example.com".to_string(), path: "/admin".to_string(), secure: false, http_only: false, same_site: String::new(), expires: None }]);
        delete_filtered(&jar, "sid", "example.com", Some("/other"));
        assert_eq!(all_cookies(&jar).len(), 1);
        delete_filtered(&jar, "sid", "example.com", Some("/admin"));
        assert!(all_cookies(&jar).is_empty());
    }

    #[test]
    fn test_delete_cookies_filtered_no_path_deletes_regardless() {
        let jar = CookieJar::new();
        set_cdp(&jar, vec![CookieInfo { name: "sid".to_string(), value: "v".to_string(), domain: "example.com".to_string(), path: "/admin".to_string(), secure: false, http_only: false, same_site: String::new(), expires: None }]);
        delete_filtered(&jar, "sid", "example.com", None);
        assert!(all_cookies(&jar).is_empty());
    }

    #[test]
    fn test_save_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cookies.json");
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "session=abc123; Domain=example.com; Path=/", &url);
        set_cookie(&jar, "token=xyz; Secure; HttpOnly", &url);
        save(&jar, &path).unwrap();
        assert!(path.exists());
        let jar2 = CookieJar::new();
        let count = load(&jar2, &path).unwrap();
        assert_eq!(count, 2);
        let header = get_header(&jar2, &url);
        assert!(header.contains("session=abc123"));
        assert!(header.contains("token=xyz"));
    }

    #[test]
    fn test_new_cookies_for_filters_http_only() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        set_cookie(&jar, "a=1; Path=/", &url);
        set_cookie(&jar, "b=2; Path=/; HttpOnly", &url);
        assert!(jar.cookies_for(&url, CookieQuery { include_http_only: true }).contains("b=2"));
        assert!(!jar.cookies_for(&url, CookieQuery { include_http_only: false }).contains("b=2"));
        assert!(jar.cookies_for(&url, CookieQuery { include_http_only: false }).contains("a=1"));
    }

    #[test]
    fn test_host_only_roundtrip_via_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cookies.json");
        let jar = CookieJar::new();
        let url = Url::parse("https://www.example.com/").unwrap();
        set_cookie(&jar, "hostonly=1; Path=/", &url); // host-only
        save(&jar, &path).unwrap();
        let jar2 = CookieJar::new();
        load(&jar2, &path).unwrap();
        assert!(get_header(&jar2, &url).contains("hostonly=1"));
        let sub = Url::parse("https://sub.www.example.com/").unwrap();
        assert!(!get_header(&jar2, &sub).contains("hostonly=1"), "host-only leaked via file");
    }

    #[test]
    fn attacker_response_cannot_set_unrelated_victim_domain_cookie() {
        let jar = CookieJar::new();
        let attacker = Url::parse("http://attacker.test/").unwrap();
        set_cookie(&jar, "sid=attacker; Domain=victim.test; Path=/", &attacker);
        let victim = Url::parse("http://victim.test/account").unwrap();
        assert!(!get_header(&jar, &victim).contains("sid=attacker"));
        assert!(get_header(&jar, &attacker).contains("sid=attacker"));
    }

    #[test]
    fn document_cookie_cannot_set_unrelated_victim_domain_cookie() {
        let jar = CookieJar::new();
        let attacker = Url::parse("http://attacker.test/").unwrap();
        set_cookie_js(&jar, "js_sid=attacker; Domain=victim.test; Path=/", &attacker);
        let victim = Url::parse("http://victim.test/account").unwrap();
        assert!(!get_header(&jar, &victim).contains("js_sid=attacker"));
    }

    #[test]
    fn public_suffix_domain_attribute_is_ignored() {
        let jar = CookieJar::new();
        let url = Url::parse("http://www.example.com/").unwrap();
        set_cookie(&jar, "bad=1; Domain=com; Path=/", &url);
        let other = Url::parse("http://other.com/").unwrap();
        assert!(!get_header(&jar, &other).contains("bad=1"));
    }

    #[test]
    fn host_only_cookie_not_sent_to_subdomain() {
        let jar = CookieJar::new();
        let www = Url::parse("http://www.example.com/").unwrap();
        set_cookie(&jar, "hostonly=1; Path=/", &www);
        assert!(get_header(&jar, &www).contains("hostonly=1"));
        let sub = Url::parse("http://sub.www.example.com/").unwrap();
        assert!(!get_header(&jar, &sub).contains("hostonly=1"));
    }

    #[test]
    fn valid_subdomain_can_set_parent_domain_cookie() {
        let jar = CookieJar::new();
        let www = Url::parse("http://www.example.com/").unwrap();
        set_cookie(&jar, "token=1; Domain=example.com; Path=/", &www);
        let apex = Url::parse("http://example.com/").unwrap();
        assert!(get_header(&jar, &apex).contains("token=1"));
        let api = Url::parse("http://api.example.com/").unwrap();
        assert!(get_header(&jar, &api).contains("token=1"));
    }

    #[test]
    fn cookie_path_requires_slash_boundary() {
        let jar = CookieJar::new();
        let admin = Url::parse("https://example.com/admin").unwrap();
        set_cookie(&jar, "sess=1; Path=/admin", &admin);
        let sibling = Url::parse("https://example.com/administrator").unwrap();
        assert!(!get_header(&jar, &sibling).contains("sess=1"));
        assert!(get_header(&jar, &admin).contains("sess=1"));
        let exact_slash = Url::parse("https://example.com/admin/").unwrap();
        assert!(get_header(&jar, &exact_slash).contains("sess=1"));
        let sub = Url::parse("https://example.com/admin/panel").unwrap();
        assert!(get_header(&jar, &sub).contains("sess=1"));
    }

    #[test]
    fn cookie_without_path_defaults_to_directory_not_full_path() {
        let jar = CookieJar::new();
        let login = Url::parse("https://example.com/app/login").unwrap();
        set_cookie(&jar, "sid=abc", &login);
        set_cookie_js(&jar, "cart=xyz", &login);
        let dashboard = Url::parse("https://example.com/app/dashboard").unwrap();
        let header = get_header(&jar, &dashboard);
        assert!(header.contains("sid=abc"));
        assert!(get_js(&jar, &dashboard).contains("cart=xyz"));
        let app_root = Url::parse("https://example.com/app/").unwrap();
        assert!(get_header(&jar, &app_root).contains("sid=abc"));
        assert!(get_header(&jar, &login).contains("sid=abc"));
        let other = Url::parse("https://example.com/other").unwrap();
        assert!(!get_header(&jar, &other).contains("sid=abc"));
    }
}
