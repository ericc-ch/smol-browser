use url::Url;

use crate::types::{NetError, RequestCredentials, RequestMode, ResourceRequest};

pub(crate) fn same_origin(request: &ResourceRequest, target: &Url) -> bool {
    request
        .initiator
        .as_ref()
        .is_some_and(|initiator| initiator.origin() == target.origin())
}

pub(crate) fn cors_required(request: &ResourceRequest, target: &Url) -> bool {
    request.mode == RequestMode::Cors && !same_origin(request, target)
}

/// Serialize the request origin used by both the Origin request header and the
/// response CORS check. A redirect chain that changes origin after it has
/// already left the initiator origin is tainted and serializes to `null`.
pub(crate) fn serialized_request_origin(
    request: &ResourceRequest,
    redirect_tainted: bool,
) -> String {
    if redirect_tainted {
        return "null".to_string();
    }
    request
        .initiator
        .as_ref()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "null".to_string())
}

pub(crate) fn redirect_taints_origin(request: &ResourceRequest, current: &Url, next: &Url) -> bool {
    current.origin() != next.origin()
        && request
            .initiator
            .as_ref()
            .is_none_or(|initiator| initiator.origin() != current.origin())
}

pub(crate) fn validate_request_mode(
    request: &ResourceRequest,
    target: &Url,
) -> Result<(), NetError> {
    if request.mode == RequestMode::SameOrigin && !same_origin(request, target) {
        return Err(NetError::Cors(format!(
            "same-origin request blocked for {}",
            target
        )));
    }
    Ok(())
}

pub(crate) fn validate_cors_response(
    request: &ResourceRequest,
    target: &Url,
    serialized_origin: &str,
    allow_origin: Option<&str>,
    allow_credentials: Option<&str>,
) -> Result<(), NetError> {
    if !cors_required(request, target) {
        return Ok(());
    }

    let allow_origin = allow_origin.ok_or_else(|| {
        NetError::Cors(format!(
            "{} did not include Access-Control-Allow-Origin for origin {}",
            target, serialized_origin
        ))
    })?;
    if request.credentials != RequestCredentials::Include && allow_origin == "*" {
        return Ok(());
    }
    if allow_origin != serialized_origin {
        return Err(NetError::Cors(format!(
            "{} returned Access-Control-Allow-Origin {:?}, expected {:?}",
            target, allow_origin, serialized_origin
        )));
    }
    if request.credentials == RequestCredentials::Include && allow_credentials != Some("true") {
        return Err(NetError::Cors(format!(
            "credentialed response from {} requires Access-Control-Allow-Credentials: true",
            target
        )));
    }
    Ok(())
}

pub(crate) fn request_fetch_site(request: &ResourceRequest, target: &Url) -> &'static str {
    let Some(initiator) = request.initiator.as_ref() else {
        return "none";
    };
    if initiator.origin() == target.origin() {
        "same-origin"
    } else {
        "cross-site"
    }
}

pub(crate) fn request_referrer(request: &ResourceRequest, target: &Url) -> Option<String> {
    let source = request.referrer.as_ref().or(request.initiator.as_ref())?;
    if !matches!(source.scheme(), "http" | "https")
        || !matches!(target.scheme(), "http" | "https")
        || (source.scheme() == "https" && target.scheme() == "http")
    {
        return None;
    }
    if source.origin() == target.origin() {
        let mut value = source.clone();
        let _ = value.set_username("");
        let _ = value.set_password(None);
        value.set_fragment(None);
        Some(value.to_string())
    } else {
        Some(format!("{}/", source.origin().ascii_serialization()))
    }
}

pub(crate) fn is_cors_simple_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "POST"
    )
}

pub(crate) fn is_cors_unsafe_request_header(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "accept" | "accept-language" | "content-language" | "content-type"
    )
}

#[cfg(test)]
mod tests {
    use super::{request_fetch_site, request_referrer, validate_cors_response};
    use crate::types::{RequestCredentials, RequestMode, ResourceRequest, ResourceType};
    use url::Url;

    #[test]
    fn subresource_referrer_and_fetch_site_follow_default_browser_policy() {
        let source = Url::parse("https://user:secret@app.example/path?q=1#frag").unwrap();
        let request = ResourceRequest::subresource(ResourceType::Image, &source);
        let same_origin = Url::parse("https://app.example/image.png").unwrap();
        let cross_origin = Url::parse("https://cdn.example/image.png").unwrap();
        let downgrade = Url::parse("http://cdn.example/image.png").unwrap();

        assert_eq!(request_fetch_site(&request, &same_origin), "same-origin");
        assert_eq!(request_fetch_site(&request, &cross_origin), "cross-site");
        assert_eq!(
            request_referrer(&request, &same_origin).as_deref(),
            Some("https://app.example/path?q=1")
        );
        assert_eq!(
            request_referrer(&request, &cross_origin).as_deref(),
            Some("https://app.example/")
        );
        assert_eq!(request_referrer(&request, &downgrade), None);
    }

    #[test]
    fn credentialed_cors_requires_exact_origin_and_allow_credentials() {
        let initiator = Url::parse("https://www.example.com/").unwrap();
        let target = Url::parse("https://api.example.com/data").unwrap();
        let mut request = ResourceRequest::subresource(ResourceType::Fetch, &initiator);
        request.mode = RequestMode::Cors;
        request.credentials = RequestCredentials::Include;

        assert!(validate_cors_response(
            &request,
            &target,
            "https://www.example.com",
            Some("*"),
            Some("true")
        )
        .is_err());
        assert!(validate_cors_response(
            &request,
            &target,
            "https://www.example.com",
            Some("https://www.example.com"),
            None
        )
        .is_err());
        assert!(validate_cors_response(
            &request,
            &target,
            "https://www.example.com",
            Some("https://www.example.com"),
            Some("true")
        )
        .is_ok());

        request.credentials = RequestCredentials::SameOrigin;
        assert!(validate_cors_response(
            &request,
            &target,
            "https://www.example.com",
            Some("*"),
            None
        )
        .is_ok());
    }
}
