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
        Self {
            inner,
            limiter: self.limiter,
        }
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
    origin_url: String,
    flaresolverr_url: Option<String>,
    session_id: Option<String>,
    default_headers: Vec<(String, String)>,
    limiter: Option<Arc<RateLimiter>>,
    /// Tracks whether direct requests work for this site.
    /// Starts `true` (optimistic). Flips to `false` after the first
    /// direct+re-solve cycle fails, so subsequent requests skip straight
    /// to the FlareSolverr proxy without wasting round-trips.
    direct_works: bool,
}

/// Public handle that is Send + Sync.
#[derive(Clone)]
pub struct FlareClient {
    inner: Arc<Mutex<Inner>>,
}

/// Heuristic: does this response body look like a Cloudflare challenge page?
fn looks_like_cf_challenge(status: u16, body: &str) -> bool {
    let lower = body.to_ascii_lowercase();

    // Cloudflare challenge pages contain characteristic markers.
    // We require at least one challenge-specific marker AND the word "cloudflare"
    // in the body, even for 403/503 status codes. A bare 403 without CF markers
    // is just a normal "forbidden" (auth, geo-block, etc.) — re-solving won't help.
    let has_cf_markers = (lower.contains("cf-browser-verification")
        || lower.contains("cf_chl_opt")
        || lower.contains("challenge-platform")
        || lower.contains("just a moment"))
        && lower.contains("cloudflare");

    if has_cf_markers {
        return true;
    }

    // Cloudflare sometimes returns very short 403/503 bodies that lack the usual
    // markers but still contain "cloudflare" in a server header rendered in the
    // page, or a turnstile script. Check for these narrower patterns only on
    // status codes that Cloudflare commonly uses for challenges.
    if (status == 403 || status == 503) && lower.contains("cloudflare") {
        return true;
    }

    false
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

    /// Re-solve via FlareSolverr and update the internal agent + headers.
    /// Returns Ok(true) if re-solve succeeded, Ok(false) if no FS configured.
    fn re_solve(&self) -> Result<bool> {
        let (fs_url, origin_url, session_id) = {
            let guard = self.inner.lock().unwrap();
            match &guard.flaresolverr_url {
                Some(url) => (
                    url.clone(),
                    guard.origin_url.clone(),
                    guard.session_id.clone(),
                ),
                None => return Ok(false),
            }
        };

        info!(
            "FlareClient: re-solving challenge via FlareSolverr for {}",
            origin_url
        );

        let solved = solve_with_flaresolverr(&fs_url, &origin_url, session_id.as_deref())?;
        let new_agent = build_ureq_agent(Some(&solved.user_agent));
        insert_flaresolverr_cookies_into_agent(&new_agent, solved.cookies);

        {
            let mut guard = self.inner.lock().unwrap();
            guard.agent = new_agent;
            guard.default_headers = solved.headers;
        }

        info!("FlareClient: re-solve succeeded, agent updated");
        Ok(true)
    }

    pub fn plain_with_rps(requests_per_second: Option<f64>) -> Self {
        let limiter = requests_per_second.and_then(RateLimiter::new).map(Arc::new);

        FlareClient {
            inner: Arc::new(Mutex::new(Inner {
                agent: build_ureq_agent(None),
                origin_url: String::new(),
                flaresolverr_url: None,
                session_id: None,
                default_headers: vec![],
                limiter,
                direct_works: true,
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
                    origin_url: origin_url.to_string(),
                    flaresolverr_url: None,
                    session_id: None,
                    default_headers: vec![],
                    limiter,
                    direct_works: true,
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
                origin_url: origin_url.to_string(),
                flaresolverr_url: Some(flaresolverr_url),
                session_id,
                default_headers,
                limiter,
                direct_works: true,
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
    ///
    /// Strategy: direct-first, proxy-on-failure with learning.
    ///   1. If direct requests have worked before (or never been tried), try a
    ///      direct GET using the current agent.
    ///   2. If challenged, re-solve via FlareSolverr and retry direct once.
    ///   3. If direct still fails, mark `direct_works = false` so future
    ///      requests skip straight to the proxy, then proxy this request.
    ///   4. On subsequent calls with `direct_works = false`, go straight to proxy.
    pub fn fetch_text(&self, url: &str) -> Result<String> {
        let (should_try_direct, has_fs) = {
            let guard = self.inner.lock().unwrap();
            (guard.direct_works, guard.flaresolverr_url.is_some())
        };

        if should_try_direct {
            // --- Attempt 1: direct GET with current agent ---
            debug!("FlareClient: direct GET {}", url);
            self.throttle();
            match self.try_direct_get(url) {
                Ok(DirectResult::Success(text)) => {
                    debug!("FlareClient: direct GET succeeded for {}", url);
                    return Ok(text);
                }
                Ok(DirectResult::Challenged(status)) => {
                    info!(
                        "FlareClient: direct GET got challenged (HTTP {}) for {}",
                        status, url
                    );
                }
                Err(e) => {
                    warn!("FlareClient: direct GET failed for {}: {:#}", url, e);
                }
            }

            // --- Attempt 2: re-solve, then retry direct GET ---
            if let Ok(true) = self.re_solve() {
                debug!(
                    "FlareClient: retrying direct GET after re-solve for {}",
                    url
                );
                self.throttle();
                match self.try_direct_get(url) {
                    Ok(DirectResult::Success(text)) => {
                        info!(
                            "FlareClient: direct GET succeeded after re-solve for {}",
                            url
                        );
                        return Ok(text);
                    }
                    Ok(DirectResult::Challenged(status)) => {
                        warn!(
                            "FlareClient: still challenged (HTTP {}) after re-solve for {}",
                            status, url
                        );
                    }
                    Err(e) => {
                        warn!(
                            "FlareClient: direct GET failed after re-solve for {}: {:#}",
                            url, e
                        );
                    }
                }
            }

            // Direct path exhausted — mark it as non-working so future
            // requests skip straight to the proxy.
            if has_fs {
                info!(
                    "FlareClient: direct path failed, switching to proxy-only for future requests"
                );
                let mut guard = self.inner.lock().unwrap();
                guard.direct_works = false;
            }
        }

        // --- Proxy path (either first attempt or fallback) ---
        let (fs_url_opt, session_id_opt) = {
            let guard = self.inner.lock().unwrap();
            (guard.flaresolverr_url.clone(), guard.session_id.clone())
        };

        if let Some(fs_url) = fs_url_opt {
            debug!("FlareClient: proxy GET {}", url);
            self.throttle();
            match proxy_fetch_text(&fs_url, session_id_opt.as_deref(), url) {
                Ok(text) => return Ok(text),
                Err(e) => {
                    warn!("FlareClient: proxy failed for {}: {:#}", url, e);
                }
            }
        }

        Err(anyhow!("FlareClient: all attempts failed for {}", url))
    }

    /// Try a direct GET and classify the result.
    fn try_direct_get(&self, url: &str) -> Result<DirectResult> {
        let (default_headers, agent) = {
            let guard = self.inner.lock().unwrap();
            (guard.default_headers.clone(), guard.agent.clone())
        };

        let req = default_headers
            .iter()
            .fold(agent.get(url), |req, (k, v)| req.header(k, v));
        let mut resp = req.call()?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string()?;

        if looks_like_cf_challenge(status, &body) {
            Ok(DirectResult::Challenged(status))
        } else {
            Ok(DirectResult::Success(body))
        }
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

        let resp_result = req.call();

        // If direct image fetch fails with a challenge, try re-solving once
        let mut resp = match resp_result {
            Ok(r) => r,
            Err(e) => {
                // Check if re-solve might help (network-level 403)
                if self.re_solve().unwrap_or(false) {
                    debug!(
                        "FlareClient: retrying image fetch after re-solve for {}",
                        url
                    );
                    let (default_headers, agent) = {
                        let guard = self.inner.lock().unwrap();
                        (guard.default_headers.clone(), guard.agent.clone())
                    };

                    self.throttle();
                    let mut retry_req = agent.get(url);
                    for (k, v) in default_headers.iter() {
                        retry_req = retry_req.header(k, v);
                    }
                    retry_req = build_image_get(url, retry_req);
                    retry_req.call()?
                } else {
                    return Err(e.into());
                }
            }
        };

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

        let data: Vec<u8> = resp
            .body_mut()
            .with_config()
            .limit(LIMIT_BYTES)
            .read_to_vec()?;
        Ok(Bytes::from(data))
    }

    /// Same direct-first strategy as fetch_text, but for POST with form data.
    pub fn post_form_text(&self, url: &str, form: &[(&str, &str)]) -> Result<String> {
        let (should_try_direct, has_fs) = {
            let guard = self.inner.lock().unwrap();
            (guard.direct_works, guard.flaresolverr_url.is_some())
        };

        if should_try_direct {
            // --- Attempt 1: direct POST ---
            debug!("FlareClient: direct POST {}", url);
            self.throttle();
            match self.try_direct_post_form(url, form) {
                Ok(DirectResult::Success(text)) => {
                    debug!("FlareClient: direct POST succeeded for {}", url);
                    return Ok(text);
                }
                Ok(DirectResult::Challenged(status)) => {
                    info!(
                        "FlareClient: direct POST got challenged (HTTP {}) for {}",
                        status, url
                    );
                }
                Err(e) => {
                    warn!("FlareClient: direct POST failed for {}: {:#}", url, e);
                }
            }

            // --- Attempt 2: re-solve, then retry direct POST ---
            if let Ok(true) = self.re_solve() {
                debug!(
                    "FlareClient: retrying direct POST after re-solve for {}",
                    url
                );
                self.throttle();
                match self.try_direct_post_form(url, form) {
                    Ok(DirectResult::Success(text)) => {
                        info!(
                            "FlareClient: direct POST succeeded after re-solve for {}",
                            url
                        );
                        return Ok(text);
                    }
                    Ok(DirectResult::Challenged(status)) => {
                        warn!(
                            "FlareClient: still challenged (HTTP {}) after re-solve for POST {}",
                            status, url
                        );
                    }
                    Err(e) => {
                        warn!(
                            "FlareClient: direct POST failed after re-solve for {}: {:#}",
                            url, e
                        );
                    }
                }
            }

            // Direct path exhausted
            if has_fs {
                info!(
                    "FlareClient: direct POST path failed, switching to proxy-only for future requests"
                );
                let mut guard = self.inner.lock().unwrap();
                guard.direct_works = false;
            }
        }

        // --- Proxy path ---
        let (fs_url_opt, session_id_opt) = {
            let guard = self.inner.lock().unwrap();
            (guard.flaresolverr_url.clone(), guard.session_id.clone())
        };

        if let Some(fs_url) = fs_url_opt {
            debug!("FlareClient: proxy POST {}", url);
            self.throttle();
            match proxy_post_form(&fs_url, session_id_opt.as_deref(), url, form) {
                Ok(body) => return Ok(body),
                Err(e) => {
                    warn!("FlareClient: proxy POST failed for {}: {:#}", url, e);
                }
            }
        }

        Err(anyhow!("FlareClient: all POST attempts failed for {}", url))
    }

    /// Try a direct POST with form data and classify the result.
    fn try_direct_post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<DirectResult> {
        let (default_headers, agent) = {
            let guard = self.inner.lock().unwrap();
            (guard.default_headers.clone(), guard.agent.clone())
        };

        let mut req = agent.post(url);
        for (k, v) in default_headers.iter() {
            req = req.header(k, v);
        }
        let mut resp = req.send_form(form.iter().copied())?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string()?;

        if looks_like_cf_challenge(status, &body) {
            Ok(DirectResult::Challenged(status))
        } else {
            Ok(DirectResult::Success(body))
        }
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

/// Result of a direct HTTP request classified by challenge detection.
enum DirectResult {
    /// Normal response body.
    Success(String),
    /// Cloudflare challenge detected; carries the HTTP status code.
    Challenged(u16),
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
        return Err(anyhow!(
            "Too many wrapper hops while fetching image: {}",
            url
        ));
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

    let data: Vec<u8> = resp
        .body_mut()
        .with_config()
        .limit(LIMIT_BYTES)
        .read_to_vec()?;
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

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn flaresolverr_url() -> String {
        env::var("FLARESOLVERR_URL").unwrap_or_else(|_| "http://localhost:8191/v1".to_string())
    }

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

    /// Build a mock FlareSolverrCookie for testing.
    fn mock_cookie(name: &str, value: &str, domain: &str) -> FlareSolverrCookie {
        FlareSolverrCookie {
            domain: domain.to_string(),
            expiry: Some((CookieOffsetDateTime::now_utc().unix_timestamp() + 3600) as u64),
            httpOnly: true,
            name: name.to_string(),
            path: "/".to_string(),
            sameSite: "Lax".to_string(),
            secure: true,
            value: value.to_string(),
        }
    }

    // =======================================================================
    // Unit tests — no network / no FlareSolverr needed
    // =======================================================================

    // --- looks_like_cf_challenge -------------------------------------------

    #[test]
    fn test_cf_challenge_detection_403_without_cf_markers() {
        // A bare 403 without any Cloudflare markers is NOT a challenge —
        // it's a normal forbidden response (auth, geo-block, etc.).
        assert!(!looks_like_cf_challenge(403, ""));
        assert!(!looks_like_cf_challenge(403, "some random body"));
        assert!(!looks_like_cf_challenge(403, "<html>Forbidden</html>"));
    }

    #[test]
    fn test_cf_challenge_detection_403_with_cloudflare() {
        // A 403 that mentions "cloudflare" IS treated as a challenge.
        assert!(looks_like_cf_challenge(
            403,
            "<html>Cloudflare: Access denied</html>"
        ));
    }

    #[test]
    fn test_cf_challenge_detection_503_without_cf_markers() {
        // Same for 503 — no Cloudflare markers means not a CF challenge.
        assert!(!looks_like_cf_challenge(503, ""));
        assert!(!looks_like_cf_challenge(503, "<html>maintenance</html>"));
    }

    #[test]
    fn test_cf_challenge_detection_503_with_cloudflare() {
        assert!(looks_like_cf_challenge(
            503,
            "<html>Service Temporarily Unavailable - Cloudflare</html>"
        ));
    }

    #[test]
    fn test_cf_challenge_detection_200_with_challenge_markers() {
        let body = r#"<html><head><title>Just a moment...</title></head>
            <body><div id="cf-browser-verification">Please wait...</div>
            Powered by Cloudflare</body></html>"#;
        assert!(looks_like_cf_challenge(200, body));
    }

    #[test]
    fn test_cf_challenge_detection_200_cf_chl_opt() {
        let body = r#"<html><script>window._cf_chl_opt={/* ... */};</script>
            <noscript>Cloudflare</noscript></html>"#;
        assert!(looks_like_cf_challenge(200, body));
    }

    #[test]
    fn test_cf_challenge_detection_200_challenge_platform() {
        let body = r#"<html><script src="/cdn-cgi/challenge-platform/scripts/jsd/main.js"></script>
            cloudflare</html>"#;
        assert!(looks_like_cf_challenge(200, body));
    }

    #[test]
    fn test_cf_challenge_detection_normal_page_not_flagged() {
        let body = "<html><body><h1>Hello World</h1></body></html>";
        assert!(!looks_like_cf_challenge(200, body));
    }

    #[test]
    fn test_cf_challenge_detection_page_mentioning_cloudflare_without_markers() {
        // Mentions "cloudflare" but none of the challenge-specific markers,
        // so it should NOT be treated as a challenge.
        let body = "<html><body>We use Cloudflare for CDN.</body></html>";
        assert!(!looks_like_cf_challenge(200, body));
    }

    #[test]
    fn test_cf_challenge_detection_case_insensitive() {
        let body = r#"<html><body>JUST A MOMENT... CLOUDFLARE</body></html>"#;
        assert!(looks_like_cf_challenge(200, body));
    }

    // --- extract_first_img_src ---------------------------------------------

    #[test]
    fn test_extract_img_src_basic() {
        let html = r#"<html><body><img src="https://example.com/image.png"></body></html>"#;
        assert_eq!(
            extract_first_img_src(html),
            Some("https://example.com/image.png".to_string())
        );
    }

    #[test]
    fn test_extract_img_src_relative() {
        let html = r#"<img src="/images/foo.jpg" alt="foo">"#;
        assert_eq!(
            extract_first_img_src(html),
            Some("/images/foo.jpg".to_string())
        );
    }

    #[test]
    fn test_extract_img_src_no_img() {
        let html = "<html><body>No images here</body></html>";
        assert_eq!(extract_first_img_src(html), None);
    }

    #[test]
    fn test_extract_img_src_picks_first() {
        let html = r#"<img src="first.png"><img src="second.png">"#;
        assert_eq!(extract_first_img_src(html), Some("first.png".to_string()));
    }

    // --- FlareSolverrResponse deserialization --------------------------------

    #[test]
    fn test_flaresolverr_response_deserialization() {
        let json_str = r#"{
            "status": "ok",
            "message": "Challenge solved!",
            "startTimestamp": 1700000000000,
            "endTimestamp": 1700000005000,
            "version": "3.3.21",
            "solution": {
                "url": "https://example.com",
                "status": 200,
                "cookies": [
                    {
                        "domain": ".example.com",
                        "expiry": 1700003600,
                        "httpOnly": true,
                        "name": "cf_clearance",
                        "path": "/",
                        "sameSite": "None",
                        "secure": true,
                        "value": "abc123"
                    }
                ],
                "userAgent": "Mozilla/5.0 Test Agent",
                "headers": {
                    "Content-Type": "text/html",
                    "X-Custom": "value"
                },
                "response": "<html>solved page</html>"
            }
        }"#;

        let parsed: FlareSolverrResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.message, "Challenge solved!");
        assert_eq!(parsed.version, "3.3.21");
        assert_eq!(parsed.solution.url, "https://example.com");
        assert_eq!(parsed.solution.status, 200);
        assert_eq!(parsed.solution.userAgent, "Mozilla/5.0 Test Agent");
        assert_eq!(parsed.solution.cookies.len(), 1);
        assert_eq!(parsed.solution.cookies[0].name, "cf_clearance");
        assert_eq!(parsed.solution.cookies[0].value, "abc123");
        assert_eq!(parsed.solution.cookies[0].domain, ".example.com");
        assert!(parsed.solution.cookies[0].httpOnly);
        assert!(parsed.solution.cookies[0].secure);
        assert_eq!(parsed.solution.cookies[0].sameSite, "None");
        assert!(parsed.solution.response.contains("solved page"));
    }

    #[test]
    fn test_flaresolverr_response_null_expiry() {
        let json_str = r#"{
            "status": "ok",
            "message": "",
            "startTimestamp": 0,
            "endTimestamp": 0,
            "version": "3.3.21",
            "solution": {
                "url": "https://example.com",
                "status": 200,
                "cookies": [
                    {
                        "domain": ".example.com",
                        "expiry": null,
                        "httpOnly": false,
                        "name": "session",
                        "path": "/",
                        "sameSite": "Lax",
                        "secure": false,
                        "value": "xyz"
                    }
                ],
                "userAgent": "UA",
                "headers": {},
                "response": ""
            }
        }"#;

        let parsed: FlareSolverrResponse = serde_json::from_str(json_str).unwrap();
        assert!(parsed.solution.cookies[0].expiry.is_none());
        assert!(!parsed.solution.cookies[0].httpOnly);
        assert!(!parsed.solution.cookies[0].secure);
    }

    #[test]
    fn test_flaresolverr_response_multiple_cookies() {
        let json_str = r#"{
            "status": "ok",
            "message": "",
            "startTimestamp": 0,
            "endTimestamp": 0,
            "version": "3.3.21",
            "solution": {
                "url": "https://example.com",
                "status": 200,
                "cookies": [
                    {"domain":".example.com","expiry":null,"httpOnly":false,"name":"a","path":"/","sameSite":"","secure":false,"value":"1"},
                    {"domain":".example.com","expiry":null,"httpOnly":true,"name":"b","path":"/sub","sameSite":"Strict","secure":true,"value":"2"},
                    {"domain":"other.com","expiry":1800000000,"httpOnly":false,"name":"c","path":"/","sameSite":"None","secure":false,"value":"3"}
                ],
                "userAgent": "UA",
                "headers": {"Accept": "text/html"},
                "response": "body"
            }
        }"#;

        let parsed: FlareSolverrResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed.solution.cookies.len(), 3);
        assert_eq!(parsed.solution.cookies[0].name, "a");
        assert_eq!(parsed.solution.cookies[1].name, "b");
        assert_eq!(parsed.solution.cookies[1].path, "/sub");
        assert_eq!(parsed.solution.cookies[1].sameSite, "Strict");
        assert_eq!(parsed.solution.cookies[2].domain, "other.com");
        assert_eq!(parsed.solution.cookies[2].expiry, Some(1800000000));
    }

    // --- insert_flaresolverr_cookies_into_agent ----------------------------

    #[test]
    fn test_insert_cookies_into_agent() {
        let agent = build_ureq_agent(Some("TestUA"));
        let cookies = vec![
            mock_cookie("cf_clearance", "test_value", ".example.com"),
            mock_cookie("session_id", "sess_abc", ".example.com"),
        ];

        // Should not panic — verifies the full cookie parsing + insertion pipeline
        insert_flaresolverr_cookies_into_agent(&agent, cookies);

        // Insert again with different values to verify overwrite doesn't panic
        let cookies2 = vec![mock_cookie("cf_clearance", "new_value", ".example.com")];
        insert_flaresolverr_cookies_into_agent(&agent, cookies2);
    }

    #[test]
    fn test_insert_cookies_domain_with_https_prefix() {
        let agent = build_ureq_agent(None);
        let cookies = vec![FlareSolverrCookie {
            domain: "https://cdn.example.com".to_string(),
            expiry: None,
            httpOnly: false,
            name: "token".to_string(),
            path: "/".to_string(),
            sameSite: "".to_string(),
            secure: false,
            value: "abc".to_string(),
        }];

        // Should not panic even with an https:// prefixed domain
        insert_flaresolverr_cookies_into_agent(&agent, cookies);
    }

    #[test]
    fn test_insert_cookies_empty_domain_fallback() {
        let agent = build_ureq_agent(None);
        let cookies = vec![FlareSolverrCookie {
            domain: "".to_string(),
            expiry: None,
            httpOnly: false,
            name: "x".to_string(),
            path: "/".to_string(),
            sameSite: "".to_string(),
            secure: false,
            value: "y".to_string(),
        }];

        // Should not panic — falls back to https://example.com
        insert_flaresolverr_cookies_into_agent(&agent, cookies);
    }

    // --- build_ureq_agent --------------------------------------------------

    #[test]
    fn test_build_ureq_agent_with_ua() {
        let agent = build_ureq_agent(Some("CustomUA/1.0"));
        // Agent is created without panic; UA is embedded in config.
        let _ = agent;
    }

    #[test]
    fn test_build_ureq_agent_no_ua() {
        let agent = build_ureq_agent(None);
        let _ = agent;
    }

    #[test]
    fn test_build_ureq_agent_empty_ua() {
        // Empty string should be skipped, not set.
        let agent = build_ureq_agent(Some(""));
        let _ = agent;
    }

    // --- FlareClient plain -------------------------------------------------

    #[test]
    fn test_plain_client_no_flaresolverr() {
        let client = FlareClient::plain();
        let guard = client.inner.lock().unwrap();
        assert!(guard.flaresolverr_url.is_none());
        assert!(guard.session_id.is_none());
        assert!(guard.default_headers.is_empty());
        assert!(guard.origin_url.is_empty());
        assert!(guard.limiter.is_none());
        assert!(guard.direct_works);
    }

    #[test]
    fn test_direct_works_starts_true() {
        let client = FlareClient::plain_with_rps(Some(1.0));
        let guard = client.inner.lock().unwrap();
        assert!(guard.direct_works, "direct_works should start as true");
    }

    #[test]
    fn test_plain_client_with_rps() {
        let client = FlareClient::plain_with_rps(Some(2.0));
        let guard = client.inner.lock().unwrap();
        assert!(guard.flaresolverr_url.is_none());
        assert!(guard.limiter.is_some());
    }

    #[test]
    fn test_plain_client_with_zero_rps() {
        // Zero or negative RPS should result in no limiter
        let client = FlareClient::plain_with_rps(Some(0.0));
        let guard = client.inner.lock().unwrap();
        assert!(guard.limiter.is_none());
    }

    #[test]
    fn test_plain_client_with_negative_rps() {
        let client = FlareClient::plain_with_rps(Some(-5.0));
        let guard = client.inner.lock().unwrap();
        assert!(guard.limiter.is_none());
    }

    #[test]
    fn test_plain_client_with_nan_rps() {
        let client = FlareClient::plain_with_rps(Some(f64::NAN));
        let guard = client.inner.lock().unwrap();
        assert!(guard.limiter.is_none());
    }

    #[test]
    fn test_plain_client_with_infinity_rps() {
        let client = FlareClient::plain_with_rps(Some(f64::INFINITY));
        let guard = client.inner.lock().unwrap();
        assert!(guard.limiter.is_none());
    }

    // --- FlareClient::from_env without FLARESOLVERR_URL set ----------------

    #[test]
    fn test_from_env_no_env_var_is_plain() {
        // Ensure env var is not set for this test
        // SAFETY: Test-only; single-threaded test runner for env-dependent tests.
        unsafe { env::remove_var("FLARESOLVERR_URL") };
        let client = FlareClient::from_env("https://example.com").unwrap();
        let guard = client.inner.lock().unwrap();
        assert!(guard.flaresolverr_url.is_none());
        assert_eq!(guard.origin_url, "https://example.com");
    }

    #[test]
    fn test_from_env_or_plain_no_env_var() {
        unsafe { env::remove_var("FLARESOLVERR_URL") };
        let client = FlareClient::from_env_or_plain("https://example.com");
        let guard = client.inner.lock().unwrap();
        assert!(guard.flaresolverr_url.is_none());
    }

    // --- FlareClient::re_solve without FS ----------------------------------

    #[test]
    fn test_re_solve_returns_false_without_flaresolverr() {
        let client = FlareClient::plain();
        let result = client.re_solve().unwrap();
        assert!(
            !result,
            "re_solve should return Ok(false) when no FS configured"
        );
    }

    // --- RateLimitedAgent --------------------------------------------------

    #[test]
    fn test_rate_limited_agent_creation() {
        let agent = build_rate_limited_ureq_agent(Some("TestUA"), Some(5.0));
        // Should have a limiter
        assert!(agent.limiter.is_some());
    }

    #[test]
    fn test_rate_limited_agent_no_limit() {
        let agent = build_rate_limited_ureq_agent(None, None);
        assert!(agent.limiter.is_none());
    }

    // --- RateLimiter -------------------------------------------------------

    #[test]
    fn test_rate_limiter_valid_rps() {
        let limiter = RateLimiter::new(10.0);
        assert!(limiter.is_some());
    }

    #[test]
    fn test_rate_limiter_zero_rps() {
        assert!(RateLimiter::new(0.0).is_none());
    }

    #[test]
    fn test_rate_limiter_negative_rps() {
        assert!(RateLimiter::new(-1.0).is_none());
    }

    #[test]
    fn test_rate_limiter_nan() {
        assert!(RateLimiter::new(f64::NAN).is_none());
    }

    #[test]
    fn test_rate_limiter_infinity() {
        assert!(RateLimiter::new(f64::INFINITY).is_none());
    }

    #[test]
    fn test_rate_limiter_acquire_does_not_block_first_call() {
        let limiter = RateLimiter::new(1000.0).unwrap(); // high RPS
        let start = std::time::Instant::now();
        limiter.acquire();
        let elapsed = start.elapsed();
        // First acquire should be near-instant
        assert!(
            elapsed.as_millis() < 50,
            "First acquire took too long: {:?}",
            elapsed
        );
    }

    #[test]
    fn test_rate_limiter_enforces_interval() {
        // 10 RPS = 100ms between requests
        let limiter = RateLimiter::new(10.0).unwrap();
        limiter.acquire(); // first: instant
        let start = std::time::Instant::now();
        limiter.acquire(); // second: should wait ~100ms
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 80, // some tolerance
            "Second acquire should have waited ~100ms, took {:?}",
            elapsed
        );
    }

    // --- build_image_get ---------------------------------------------------

    #[test]
    fn test_build_image_get_sets_accept_header() {
        let agent = build_ureq_agent(None);
        let req = agent.get("https://cdn.example.com/image.png");
        let req = build_image_get("https://cdn.example.com/image.png", req);
        // We can't easily inspect headers on the builder, but we verify
        // it doesn't panic and the request can be built.
        let _ = req;
    }

    // --- DirectResult via try_direct_get -----------------------------------

    #[test]
    fn test_direct_result_enum_variants() {
        // Ensure the enum is constructable (compile-time check mostly)
        let success = DirectResult::Success("hello".to_string());
        let challenged = DirectResult::Challenged(403);

        match success {
            DirectResult::Success(s) => assert_eq!(s, "hello"),
            _ => panic!("Expected Success"),
        }

        match challenged {
            DirectResult::Challenged(code) => assert_eq!(code, 403),
            _ => panic!("Expected Challenged"),
        }
    }

    // --- build_rate_limited_flaresolverr_client without env -----------------

    #[test]
    fn test_build_rate_limited_flaresolverr_client_no_env() {
        unsafe { env::remove_var("FLARESOLVERR_URL") };
        // Should fall back to plain client without panicking
        let client = build_rate_limited_flaresolverr_client("https://example.com", Some(3.0));
        let guard = client.inner.lock().unwrap();
        assert!(guard.flaresolverr_url.is_none());
        assert!(guard.limiter.is_some());
    }

    // --- FlareClient is Clone + Send + Sync --------------------------------

    #[test]
    fn test_flare_client_is_clone_send_sync() {
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<FlareClient>();
    }

    #[test]
    fn test_rate_limited_agent_is_clone_send_sync() {
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<RateLimitedAgent>();
    }

    // --- Thread safety: concurrent access ----------------------------------

    #[test]
    fn test_flare_client_concurrent_re_solve_no_panic() {
        // Without FS configured, re_solve returns Ok(false).
        // Verify no deadlocks under concurrent access.
        let client = FlareClient::plain();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let c = client.clone();
                std::thread::spawn(move || {
                    let result = c.re_solve().unwrap();
                    assert!(!result);
                })
            })
            .collect();

        for h in handles {
            h.join().expect("Thread panicked");
        }
    }

    // =======================================================================
    // Integration tests — require running FlareSolverr + network access
    // Run with: cargo test -- --ignored
    // =======================================================================

    #[test]
    #[ignore]
    fn test_nowsecure() {
        let fs_url = flaresolverr_url();

        let flare_body = get_flaresolverr_response("https://nowsecure.com", &fs_url);
        assert_eq!(flare_body.status, "ok");
        assert!(!flare_body.solution.response.is_empty());
        assert!(!flare_body.solution.userAgent.is_empty());
        assert!(!flare_body.solution.cookies.is_empty());

        let ureq_body = get_ureq_response("https://nowsecure.com", &fs_url);
        assert!(!ureq_body.is_empty());
    }

    #[test]
    #[ignore]
    fn test_openai() {
        let fs_url = flaresolverr_url();

        let flare_body = get_flaresolverr_response("https://openai.com", &fs_url);
        assert_eq!(flare_body.status, "ok");
        assert!(!flare_body.solution.response.is_empty());

        let ureq_body = get_ureq_response("https://openai.com", &fs_url);
        assert!(!ureq_body.is_empty());
    }

    /// Integration: FlareClient direct-first strategy against a CF-protected site.
    /// Validates that:
    ///   1. from_env solves initially and stores cookies/UA/headers
    ///   2. fetch_text succeeds via the direct path (no per-request proxy)
    ///   3. The returned HTML is the real page, not a challenge
    #[test]
    #[ignore]
    fn test_flare_client_direct_first_fetch() {
        let fs_url = flaresolverr_url();
        unsafe { env::set_var("FLARESOLVERR_URL", &fs_url) };

        let client = FlareClient::from_env("https://nowsecure.com").unwrap();

        // Verify internal state was populated by the initial solve
        {
            let guard = client.inner.lock().unwrap();
            assert!(guard.flaresolverr_url.is_some());
            assert_eq!(guard.origin_url, "https://nowsecure.com");
            // After a successful solve, we should have default_headers
            // (may be empty if the site returns few headers, but agent
            // should have cookies).
        }

        let body = client.fetch_text("https://nowsecure.com").unwrap();
        assert!(!body.is_empty());
        // The body should NOT be a challenge page
        assert!(
            !looks_like_cf_challenge(200, &body),
            "fetch_text returned a challenge page instead of the real content"
        );
    }

    /// Integration: FlareClient.fetch_bytes for image fetching
    #[test]
    #[ignore]
    fn test_flare_client_fetch_bytes() {
        // Use a known public image URL (not CF-protected, just validates
        // the fetch_bytes pipeline works end-to-end).
        let client = FlareClient::plain();
        let bytes = client.fetch_bytes("https://httpbin.org/image/png").unwrap();
        assert!(!bytes.is_empty());
        // PNG magic bytes
        assert_eq!(&bytes[0..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    /// Integration: RateLimitedAgent.fetch_bytes
    #[test]
    #[ignore]
    fn test_rate_limited_agent_fetch_bytes() {
        let agent = build_rate_limited_ureq_agent(None, Some(5.0));
        let bytes = agent.fetch_bytes("https://httpbin.org/image/png").unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[0..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    /// Integration: solve_with_flaresolverr returns proper Solved struct
    #[test]
    #[ignore]
    fn test_solve_with_flaresolverr_struct() {
        let fs_url = flaresolverr_url();
        let solved = solve_with_flaresolverr(&fs_url, "https://nowsecure.com", None).unwrap();

        assert!(
            !solved.user_agent.is_empty(),
            "user_agent should not be empty"
        );
        assert!(!solved.cookies.is_empty(), "should have received cookies");

        // At least one cookie should be cf_clearance
        let has_clearance = solved.cookies.iter().any(|c| c.name == "cf_clearance");
        assert!(
            has_clearance,
            "Expected cf_clearance cookie in solved cookies: {:?}",
            solved.cookies.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
    }

    /// Integration: FlareClient.post_form_text with direct-first strategy
    #[test]
    #[ignore]
    fn test_flare_client_post_form() {
        // httpbin echoes back form data — validates the POST pipeline
        let client = FlareClient::plain();
        let body = client
            .post_form_text(
                "https://httpbin.org/post",
                &[("key", "value"), ("foo", "bar")],
            )
            .unwrap();

        assert!(body.contains("key"));
        assert!(body.contains("value"));
        assert!(body.contains("foo"));
        assert!(body.contains("bar"));
    }

    /// Integration: FlareClient.post_empty_text
    #[test]
    #[ignore]
    fn test_flare_client_post_empty() {
        let client = FlareClient::plain();
        let body = client
            .post_empty_text("https://httpbin.org/post", &[("X-Custom", "hello")])
            .unwrap();

        assert!(!body.is_empty());
        assert!(body.contains("X-Custom"));
    }

    /// Integration: FlareClient with session support
    #[test]
    #[ignore]
    fn test_flare_client_session_creation() {
        let fs_url = flaresolverr_url();
        unsafe { env::set_var("FLARESOLVERR_URL", &fs_url) };
        unsafe { env::remove_var("FLARESOLVERR_SESSION") };

        let client = FlareClient::from_env("https://nowsecure.com").unwrap();
        let guard = client.inner.lock().unwrap();

        // Session should have been auto-created
        assert!(
            guard.session_id.is_some(),
            "Expected a session_id to be created automatically"
        );
    }

    /// Integration: multiple sequential fetches reuse the same agent (direct path)
    #[test]
    #[ignore]
    fn test_flare_client_multiple_fetches_reuse_agent() {
        let fs_url = flaresolverr_url();
        unsafe { env::set_var("FLARESOLVERR_URL", &fs_url) };

        let client = FlareClient::from_env("https://nowsecure.com").unwrap();

        // Fetch the same URL multiple times — all should succeed via direct path
        for i in 0..3 {
            let body = client.fetch_text("https://nowsecure.com").unwrap();
            assert!(!body.is_empty(), "Fetch #{} returned empty body", i + 1);
            assert!(
                !looks_like_cf_challenge(200, &body),
                "Fetch #{} returned a challenge page",
                i + 1
            );
        }
    }
}
