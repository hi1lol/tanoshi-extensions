mod ratelimit;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use cookie::time::OffsetDateTime as CookieOffsetDateTime;
use log::{debug, info, warn};
use ratelimit::RateLimiter;
use serde_json::{Value as JsonValue, json};
use std::error::Error;
use std::sync::{Arc, Mutex};
use ureq::typestate::{WithBody, WithoutBody};
use ureq::{
    Cookie,
    http::{Uri, header::CONTENT_TYPE},
};
use url::Url;

pub type Agent = ureq::Agent;

pub type HttpResponse = ureq::http::Response<ureq::Body>;

const LIMIT_BYTES: u64 = 50 * 1024 * 1024; // 50 MiB

#[derive(Clone)]
pub struct RateLimitedAgent {
    inner: ureq::Agent,
    limiter: Option<Arc<RateLimiter>>,
}

impl RateLimitedAgent {
    pub fn new(inner: ureq::Agent, requests_per_second: Option<f64>) -> Self {
        info!(
            "Net RateLimitedAgent setup with {:?} RPS",
            requests_per_second
        );
        let limiter = requests_per_second.and_then(RateLimiter::new).map(Arc::new);

        Self { inner, limiter }
    }

    pub fn get(&self, url: &str) -> RateLimitedRequest<WithoutBody> {
        RateLimitedRequest {
            inner: self.inner.get(url),
            limiter: self.limiter.clone(),
        }
    }

    pub fn post(&self, url: &str) -> RateLimitedRequest<WithBody> {
        RateLimitedRequest {
            inner: self.inner.post(url),
            limiter: self.limiter.clone(),
        }
    }
    
    pub fn fetch_bytes(&self, url: &str) -> anyhow::Result<Bytes> {
        let mut getter = |u: &str| -> anyhow::Result<ureq::http::Response<ureq::Body>> {
            Ok(self.get(u).image_defaults(u).call()?)
        };

        bytes_fetch_impl(&mut getter, url, 0)
    }
}

pub struct RateLimitedRequest<B> {
    inner: ureq::RequestBuilder<B>,
    limiter: Option<Arc<RateLimiter>>,
}

impl<B> RateLimitedRequest<B> {
    #[inline]
    fn throttle(&self) {
        if let Some(l) = &self.limiter {
            l.acquire();
        }
    }

    pub fn header<K, V>(self, key: K, value: V) -> Self
    where
        ureq::http::header::HeaderName: TryFrom<K>,
        <ureq::http::header::HeaderName as TryFrom<K>>::Error: Into<ureq::http::Error>,
        ureq::http::header::HeaderValue: TryFrom<V>,
        <ureq::http::header::HeaderValue as TryFrom<V>>::Error: Into<ureq::http::Error>,
    {
        Self {
            inner: self.inner.header(key, value),
            limiter: self.limiter,
        }
    }

    pub fn query<K, V>(self, key: K, value: V) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        Self {
            inner: self.inner.query(key, value),
            limiter: self.limiter,
        }
    }
}

/// Methods only available for RequestBuilder<WithoutBody>
impl RateLimitedRequest<WithoutBody> {
    pub fn image_defaults(self, url: &str) -> Self {
        let inner = build_image_get(url, self.inner);
        Self { inner, limiter: self.limiter }
    }

    pub fn call(self) -> Result<HttpResponse, ureq::Error> {
        self.throttle();
        self.inner.call()
    }
}

/// Methods only available for RequestBuilder<WithBody>
impl RateLimitedRequest<WithBody> {
    pub fn send_empty(self) -> Result<HttpResponse, ureq::Error> {
        self.throttle();
        self.inner.send_empty()
    }

    pub fn send_form<I, K, V>(self, form: I) -> Result<HttpResponse, ureq::Error>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.throttle();
        self.inner.send_form(form)
    }

    pub fn send_json<T: serde::Serialize>(self, value: &T) -> Result<HttpResponse, ureq::Error> {
        self.throttle();
        self.inner.send_json(value)
    }
}
#[allow(non_snake_case)]
#[derive(Debug, serde::Deserialize, Clone)]
pub struct FlareSolverrResponse {
    pub status: String,
    pub message: String,
    pub solution: FlareSolverrSolution,
    pub startTimestamp: u64,
    pub endTimestamp: u64,
    pub version: String,
}

#[allow(non_snake_case)]
#[derive(Debug, serde::Deserialize, Clone)]
pub struct FlareSolverrSolution {
    pub url: String,
    pub status: u16,
    pub cookies: Vec<FlareSolverrCookie>,
    pub userAgent: String,
    pub headers: JsonValue,
    pub response: String,
}

#[allow(non_snake_case)]
#[derive(Debug, serde::Deserialize, Clone)]
pub struct FlareSolverrCookie {
    pub domain: String,
    pub expiry: Option<u64>,
    pub httpOnly: bool,
    pub name: String,
    pub path: String,
    pub sameSite: String,
    pub secure: bool,
    pub value: String,
}

pub fn build_ureq_agent(user_agent: Option<&str>) -> Agent {
    let mut cfg = Agent::config_builder().max_redirects(5);
    if let Some(ua) = user_agent {
        if !ua.is_empty() {
            cfg = cfg.user_agent(ua);
        }
    }
    cfg.build().into()
}

pub fn build_rate_limited_ureq_agent(
    user_agent: Option<&str>,
    requests_per_second: Option<f64>,
) -> RateLimitedAgent {
    let agent = build_ureq_agent(user_agent);
    RateLimitedAgent::new(agent, requests_per_second)
}

pub fn build_rate_limited_flaresolverr_client(
    origin_url: &str,
    requests_per_second: Option<f64>,
) -> FlareClient {
    FlareClient::from_env_with_rps(origin_url, requests_per_second)
        .unwrap_or_else(|_| FlareClient::plain_with_rps(requests_per_second))
}

fn insert_flaresolverr_cookies_into_agent(agent: &Agent, cookies: Vec<FlareSolverrCookie>) {
    let mut jar = agent.cookie_jar_lock();
    for c in cookies {
        let mut parts = vec![format!("{}={}", c.name, c.value)];

        // Path
        if !c.path.is_empty() {
            parts.push(format!("Path={}", c.path));
        }
        // Domain
        if !c.domain.is_empty() {
            parts.push(format!("Domain={}", c.domain));
        }
        // Secure / HttpOnly
        if c.secure {
            parts.push("Secure".to_string());
        }
        if c.httpOnly {
            parts.push("HttpOnly".to_string());
        }
        // SameSite
        match c.sameSite.as_str() {
            "Strict" | "Lax" | "None" => parts.push(format!("SameSite={}", c.sameSite)),
            _ => {}
        }
        // Expires
        if let Some(expiry) = c.expiry {
            if let Ok(ts) = CookieOffsetDateTime::from_unix_timestamp(expiry as i64) {
                let max_age =
                    ts.unix_timestamp() - CookieOffsetDateTime::now_utc().unix_timestamp();
                if max_age > 0 {
                    parts.push(format!("Max-Age={}", max_age));
                }
            }
        }

        let set_cookie = parts.join("; ");

        // Bind parse to a relevant URI (scheme/host are used for defaults)
        let uri_str = if c.domain.starts_with("http://") || c.domain.starts_with("https://") {
            c.domain.clone()
        } else {
            format!("https://{}", c.domain)
        };
        // Fallback: if domain is empty, use a dummy host.
        let uri = Uri::try_from(uri_str.as_str())
            .unwrap_or_else(|_| Uri::from_static("https://example.com"));

        if let Ok(cookie) = Cookie::parse(set_cookie, &uri) {
            let _ = jar.insert(cookie, &uri);
        }
    }
    jar.release();
}

pub fn build_flaresolverr_client(
    url: &str,
    flaresolverr_url: &str,
) -> Result<Agent, Box<dyn Error>> {
    let payload = json!({
        "cmd": "request.get",
        "url": url,
        "maxTimeout": 60000,
    });

    let mut response = ureq::post(flaresolverr_url)
        .header("Content-Type", "application/json")
        .send_json(&payload)?;

    let text = response.body_mut().read_to_string()?;
    let body: FlareSolverrResponse = serde_json::from_str(&text)?;
    if body.status != "ok" {
        return Err(format!("FlareSolverr error: {}", body.message).into());
    }

    let user_agent = body.solution.userAgent.clone();
    let agent = build_ureq_agent(Some(&user_agent));

    insert_flaresolverr_cookies_into_agent(&agent, body.solution.cookies);

    Ok(agent)
}

/// Internal, mutable state wrapped by a Mutex.
#[derive(Clone)]
struct Inner {
    agent: Agent,
    flaresolverr_url: Option<String>,
    session_id: Option<String>,
    default_headers: Vec<(String, String)>,
    limiter: Option<Arc<RateLimiter>>,
}

/// Public handle that is Send + Sync.
#[derive(Clone)]
pub struct FlareClient {
    inner: Arc<Mutex<Inner>>,
}
impl FlareClient {
    #[inline]
    fn throttle(&self) {
        let limiter = {
            let guard = self.inner.lock().unwrap();
            guard.limiter.clone()
        };
        if let Some(l) = limiter {
            l.acquire();
        }
    }

    pub fn plain_with_rps(requests_per_second: Option<f64>) -> Self {
        let limiter = requests_per_second.and_then(RateLimiter::new).map(Arc::new);

        FlareClient {
            inner: Arc::new(Mutex::new(Inner {
                agent: build_ureq_agent(None),
                flaresolverr_url: None,
                session_id: None,
                default_headers: vec![],
                limiter,
            })),
        }
    }

    pub fn from_env_with_rps(origin_url: &str, requests_per_second: Option<f64>) -> Result<Self> {
        let limiter = requests_per_second.and_then(RateLimiter::new).map(Arc::new);

        let flaresolverr_url = std::env::var("FLARESOLVERR_URL").ok();
        debug!("FLARESOLVERR_URL={:?}", flaresolverr_url);

        if flaresolverr_url.is_none() {
            return Ok(Self {
                inner: Arc::new(Mutex::new(Inner {
                    agent: build_ureq_agent(None),
                    flaresolverr_url: None,
                    session_id: None,
                    default_headers: vec![],
                    limiter,
                })),
            });
        }

        let flaresolverr_url = flaresolverr_url.unwrap();
        // Optional session
        let mut session_id = std::env::var("FLARESOLVERR_SESSION").ok();
        if session_id.is_none() {
            if let Ok(mut resp) = ureq::post(&flaresolverr_url)
                .header("Content-Type", "application/json")
                .send_json(&json!({"cmd":"sessions.create"}))
            {
                if let Ok(text) = resp.body_mut().read_to_string() {
                    #[derive(serde::Deserialize)]
                    struct Created {
                        status: String,
                        session: Option<String>,
                    }
                    if let Ok(Created { status, session }) = serde_json::from_str(&text) {
                        if status == "ok" {
                            session_id = session;
                        }
                    }
                }
            }
        }

        // Try initial solve; on failure fall back to plain agent.
        let (agent, default_headers) =
            match solve_with_flaresolverr(&flaresolverr_url, origin_url, session_id.as_deref()) {
                Ok(solved) => {
                    let a = build_ureq_agent(Some(&solved.user_agent));
                    insert_flaresolverr_cookies_into_agent(&a, solved.cookies);
                    (a, solved.headers)
                }
                Err(_) => (build_ureq_agent(None), vec![]),
            };

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                agent,
                flaresolverr_url: Some(flaresolverr_url),
                session_id,
                default_headers,
                limiter,
            })),
        })
    }

    /// Plain client (no FlareSolverr), safe default.
    pub fn plain() -> Self {
        Self::plain_with_rps(None)
    }

    /// Make this never error: on any failure, return a plain client.
    pub fn from_env_or_plain(origin_url: &str) -> Self {
        Self::from_env_with_rps(origin_url, None).unwrap_or_else(|_| Self::plain_with_rps(None))
    }

    pub fn from_env(origin_url: &str) -> Result<Self> {
        Self::from_env_with_rps(origin_url, None)
    }

    /// Thread-safe: takes &self. Internally locks, mutates as needed.
    pub fn fetch_text(&self, url: &str) -> Result<String> {
        // Fast path: copy FS config to avoid holding the lock during network I/O
        let (fs_url_opt, session_id_opt, default_headers) = {
            let guard = self.inner.lock().unwrap();
            (
                guard.flaresolverr_url.clone(),
                guard.session_id.clone(),
                guard.default_headers.clone(),
            )
        };

        // If FS configured, try it first.
        if let Some(fs_url) = fs_url_opt {
            self.throttle();
            match proxy_fetch_text(&fs_url, session_id_opt.as_deref(), url) {
                Ok(text) => {
                    info!("FlareClient: proxied GET {}", url);
                    return Ok(text);
                }
                Err(e) => {
                    warn!("FlareClient: proxy failed for {}: {:#}", url, e);
                }
            }
        }

        // Direct GET using current agent + headers
        debug!("FlareClient: direct GET {}", url);
        self.throttle();
        direct_get_with_headers(self, url, &default_headers)
    }

    pub fn get_text(&self, url: &str) -> Result<String> {
        self.fetch_text(url)
    }

    pub fn fetch_bytes(&self, url: &str) -> Result<Bytes> {
        self.fetch_bytes_inner(url, 0)
    }

    fn fetch_bytes_inner(&self, url: &str, depth: u8) -> Result<Bytes> {
        if depth > 2 {
            return Err(anyhow!(
                "Too many wrapper hops while fetching image: {}",
                url
            ));
        }

        let (default_headers, agent) = {
            let guard = self.inner.lock().unwrap();
            (guard.default_headers.clone(), guard.agent.clone())
        };

        self.throttle();
        let mut req = agent.get(url);

        for (k, v) in default_headers.iter() {
            req = req.header(k, v);
        }

        req = build_image_get(url, req);

        let mut resp = req.call()?;

        let status = resp.status();
        if status.as_u16() >= 400 {
            return Err(anyhow!(
                "Image fetch failed: HTTP {} for {}",
                status.as_u16(),
                url
            ));
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if content_type.starts_with("text/html") {
            let html = resp.body_mut().read_to_string()?;

            if let Some(next) = extract_first_img_src(&html) {
                let next_url = match Url::parse(url).ok().and_then(|base| base.join(&next).ok()) {
                    Some(u) => u.to_string(),
                    None => next,
                };
                return self.fetch_bytes_inner(&next_url, depth + 1);
            }

            return Err(anyhow!(
                "Expected image bytes but got HTML and no <img src=...> found for {}",
                url
            ));
        }

        let data: Vec<u8> = resp.body_mut().with_config().limit(LIMIT_BYTES).read_to_vec()?;
        Ok(Bytes::from(data))
    }

    pub fn post_form_text(&self, url: &str, form: &[(&str, &str)]) -> Result<String> {
        // snapshot config
        let (fs_url_opt, session_id_opt, default_headers) = {
            let guard = self.inner.lock().unwrap();
            (
                guard.flaresolverr_url.clone(),
                guard.session_id.clone(),
                guard.default_headers.clone(),
            )
        };

        // Try FlareSolverr first
        if let Some(fs_url) = fs_url_opt {
            self.throttle();
            match proxy_post_form(&fs_url, session_id_opt.as_deref(), url, form) {
                Ok(body) => {
                    info!("FlareClient: proxied POST {}", url);
                    return Ok(body);
                }
                Err(e) => {
                    warn!("FlareClient: proxy failed for {}: {:#}", url, e);
                }
            }
        }

        // Fallback to direct POST
        debug!("FlareClient: direct POST {}", url);
        self.throttle();
        let agent = {
            let guard = self.inner.lock().unwrap();
            guard.agent.clone()
        };
        let mut req = agent.post(url);
        for (k, v) in default_headers {
            req = req.header(&k, &v);
        }
        let mut resp = req.send_form(form.iter().copied())?;
        Ok(resp.body_mut().read_to_string()?)
    }

    pub fn post_empty_text(&self, url: &str, extra_headers: &[(&str, &str)]) -> Result<String> {
        self.throttle();

        // Snapshot default headers and agent
        let (default_headers, agent) = {
            let guard = self.inner.lock().unwrap();
            (guard.default_headers.clone(), guard.agent.clone())
        };

        let mut req = agent.post(url);

        // default_headers: Vec<(String, String)>
        for (k, v) in default_headers.iter() {
            req = req.header(k, v); // &String → &str via Deref
        }

        // extra_headers: &[(&str, &str)]
        for (k, v) in extra_headers.iter() {
            req = req.header(*k, *v); // &(&str) → &str
        }

        let mut resp = req.send_empty()?;
        Ok(resp.body_mut().read_to_string()?)
    }
}

fn proxy_fetch_text(fs_url: &str, session_id: Option<&str>, url: &str) -> Result<String> {
    let payload = match session_id {
        Some(sid) => json!({"cmd":"request.get","url":url,"maxTimeout":60000,"session":sid}),
        None => json!({"cmd":"request.get","url":url,"maxTimeout":60000}),
    };

    let mut resp = ureq::post(fs_url)
        .header("Content-Type", "application/json")
        .send_json(&payload)?;
    let text = resp.body_mut().read_to_string()?;
    let body: FlareSolverrResponse = serde_json::from_str(&text)?;
    if body.status != "ok" {
        return Err(anyhow!("FlareSolverr error: {}", body.message));
    }
    Ok(body.solution.response)
}

fn proxy_post_form(
    fs_url: &str,
    session_id: Option<&str>,
    url: &str,
    form: &[(&str, &str)],
) -> Result<String> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let payload = match session_id {
        Some(sid) => json!({
            "cmd": "request.post",
            "url": url,
            "maxTimeout": 60000,
            "session": sid,
            "postData": body,
        }),
        None => json!({
            "cmd": "request.post",
            "url": url,
            "maxTimeout": 60000,
            "postData": body,
        }),
    };

    let mut resp = ureq::post(fs_url)
        .header("Content-Type", "application/json")
        .send_json(&payload)?;
    let text = resp.body_mut().read_to_string()?;
    let body: FlareSolverrResponse = serde_json::from_str(&text)?;
    if body.status != "ok" {
        return Err(anyhow!("FlareSolverr error: {}", body.message));
    }
    Ok(body.solution.response)
}

fn direct_get_with_headers(
    client: &FlareClient,
    url: &str,
    default_headers: &[(String, String)],
) -> Result<String> {
    // Build the request with headers on the fly; only need read access for agent
    let agent = {
        let guard = client.inner.lock().unwrap();
        guard.agent.clone()
    };

    let req = default_headers
        .iter()
        .fold(agent.get(url), |req, (k, v)| req.header(k, v));
    let mut resp = req.call()?;
    Ok(resp.body_mut().read_to_string()?)
}

struct Solved {
    user_agent: String,
    cookies: Vec<FlareSolverrCookie>,
    headers: Vec<(String, String)>,
}

fn solve_with_flaresolverr(
    flaresolverr_url: &str,
    url: &str,
    session: Option<&str>,
) -> Result<Solved> {
    let payload = match session {
        Some(sid) => json!({"cmd":"request.get","url":url,"maxTimeout":60000,"session":sid}),
        None => json!({"cmd":"request.get","url":url,"maxTimeout":60000}),
    };

    let mut response = ureq::post(flaresolverr_url)
        .header("Content-Type", "application/json")
        .send_json(&payload)?;
    let text = response.body_mut().read_to_string()?;
    let body: FlareSolverrResponse = serde_json::from_str(&text)?;
    if body.status != "ok" {
        return Err(anyhow!("FlareSolverr error: {}", body.message));
    }

    let mut hdrs: Vec<(String, String)> = vec![];
    if let Some(obj) = body.solution.headers.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                if !k.eq_ignore_ascii_case("set-cookie") {
                    hdrs.push((k.to_string(), s.to_string()));
                }
            } else {
                hdrs.push((k.to_string(), v.to_string()));
            }
        }
    }

    Ok(Solved {
        user_agent: body.solution.userAgent.clone(),
        cookies: body.solution.cookies,
        headers: hdrs,
    })
}

const IMAGE_ACCEPT: &str = "image/avif,image/webp,image/apng,image/*,*/*;q=0.8";

fn build_image_get(
    url: &str,
    mut req: ureq::RequestBuilder<WithoutBody>,
) -> ureq::RequestBuilder<WithoutBody> {
    // Build a referer from the target URL origin
    let referer = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("{}://{}/", u.scheme(), h)));

    req = req.header("Accept", IMAGE_ACCEPT);

    if let Some(r) = referer {
        req = req.header("Referer", r);
    }

    req
}

fn bytes_fetch_impl<F>(do_get: &mut F, url: &str, depth: u8) -> anyhow::Result<Bytes>
where
    F: FnMut(&str) -> anyhow::Result<ureq::http::Response<ureq::Body>>,
{
    if depth > 2 {
        return Err(anyhow!("Too many wrapper hops while fetching image: {}", url));
    }

    let mut resp = do_get(url)?;

    let status = resp.status();
    if status.as_u16() >= 400 {
        return Err(anyhow!(
            "Image fetch failed: HTTP {} for {}",
            status.as_u16(),
            url
        ));
    }

    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if content_type.starts_with("text/html") {
        let html = resp.body_mut().read_to_string()?;
        if let Some(next) = extract_first_img_src(&html) {
            let next_url = match Url::parse(url).ok().and_then(|base| base.join(&next).ok()) {
                Some(u) => u.to_string(),
                None => next,
            };
            return bytes_fetch_impl(do_get, &next_url, depth + 1);
        }

        return Err(anyhow!(
            "Expected image bytes but got HTML and no <img src=...> found for {}",
            url
        ));
    }

    let data: Vec<u8> = resp.body_mut().with_config().limit(LIMIT_BYTES).read_to_vec()?;
    Ok(Bytes::from(data))
}

// Tiny helper: pull the first <img ... src="..."> out of wrapper HTML.
fn extract_first_img_src(html: &str) -> Option<String> {
    // Look for: src="...".
    let needle = "src=\"";
    let start = html.find(needle)? + needle.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod test {
    use super::*;
    use serde_json::json;
    use std::env;

    fn get_flaresolverr_response(url: &str, flaresolverr_url: &str) -> FlareSolverrResponse {
        let payload = json!({
            "cmd": "request.get",
            "url": url,
            "maxTimeout": 60000,
        });

        let flare_response = ureq::post(flaresolverr_url)
            .header("Content-Type", "application/json")
            .send_json(&payload);

        assert!(flare_response.is_ok());
        let mut resp = flare_response.unwrap();
        let text = resp.body_mut().read_to_string().unwrap();
        serde_json::from_str::<FlareSolverrResponse>(&text).unwrap()
    }

    fn get_ureq_response(url: &str, flaresolverr_url: &str) -> String {
        let client = build_flaresolverr_client(url, flaresolverr_url).unwrap();
        let resp = client.get(url).call();

        if let Err(e) = &resp {
            eprintln!("Error making request: {}", e);
        }

        assert!(resp.is_ok());
        let mut r = resp.unwrap();
        r.body_mut().read_to_string().unwrap()
    }

    #[test]
    #[ignore]
    fn test_nowsecure() {
        let flaresolverr_url =
            env::var("FLARESOLVERR_URL").unwrap_or_else(|_| "http://localhost:8191/v1".to_string());

        let flare_body = get_flaresolverr_response("https://nowsecure.com", &flaresolverr_url);
        assert!(!flare_body.solution.response.is_empty());

        let ureq_body = get_ureq_response("https://nowsecure.com", &flaresolverr_url);
        assert!(!ureq_body.is_empty());
    }

    #[test]
    #[ignore]
    fn test_openai() {
        let flaresolverr_url =
            env::var("FLARESOLVERR_URL").unwrap_or_else(|_| "http://localhost:8191/v1".to_string());

        let flare_body = get_flaresolverr_response("https://openai.com", &flaresolverr_url);
        assert!(!flare_body.solution.response.is_empty());

        let ureq_body = get_ureq_response("https://openai.com", &flaresolverr_url);
        assert!(!ureq_body.is_empty());
    }
}
