use anyhow::{Result, anyhow};
use cookie::time::OffsetDateTime as CookieOffsetDateTime;
use serde_json::{Value as JsonValue, json};
use std::error::Error;
use std::sync::{Arc, Mutex};
use ureq::{Cookie, http::Uri};

pub type Agent = ureq::Agent;

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
}

/// Public handle that is Send + Sync.
#[derive(Clone)]
pub struct FlareClient {
    inner: Arc<Mutex<Inner>>,
}
impl FlareClient {
    /// Plain client (no FlareSolverr), safe default.
    pub fn plain() -> Self {
        FlareClient {
            inner: std::sync::Arc::new(std::sync::Mutex::new(Inner {
                agent: build_ureq_agent(None),
                flaresolverr_url: None,
                session_id: None,
                default_headers: vec![],
            })),
        }
    }

    /// Make this never error: on any failure, return a plain client.
    pub fn from_env_or_plain(origin_url: &str) -> Self {
        Self::from_env(origin_url).unwrap_or_else(|_| Self::plain())
    }
    
    pub fn from_env(origin_url: &str) -> Result<Self> {
        let flaresolverr_url = std::env::var("FLARESOLVERR_URL").ok();

        // If FS not configured: plain agent, no headers.
        if flaresolverr_url.is_none() {
            return Ok(Self {
                inner: Arc::new(Mutex::new(Inner {
                    agent: build_ureq_agent(None),
                    flaresolverr_url: None,
                    session_id: None,
                    default_headers: vec![],
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
            })),
        })
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
            match proxy_fetch_text(&fs_url, session_id_opt.as_deref(), url) {
                Ok(text) => return Ok(text),
                Err(_e) => {
                    // fall through to direct
                }
            }
        }

        // Direct GET using current agent + headers
        direct_get_with_headers(self, url, &default_headers)
    }

    pub fn get_text(&self, url: &str) -> Result<String> {
        self.fetch_text(url)
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
            if let Ok(body) = proxy_post_form(&fs_url, session_id_opt.as_deref(), url, form) {
                return Ok(body);
            }
        }

        // Fallback to direct POST
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

    pub fn post_empty_text(
        &self,
        url: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<String> {
        // Snapshot default headers and agent
        let (default_headers, agent) = {
            let guard = self.inner.lock().unwrap();
            (guard.default_headers.clone(), guard.agent.clone())
        };

        let mut req = agent.post(url);

        // default_headers: Vec<(String, String)>
        for (k, v) in default_headers.iter() {
            req = req.header(k, v);            // &String → &str via Deref
        }

        // extra_headers: &[(&str, &str)]
        for (k, v) in extra_headers.iter() {
            req = req.header(*k, *v);          // &(&str) → &str
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
