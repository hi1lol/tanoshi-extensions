use cookie::time::OffsetDateTime as CookieOffsetDateTime;
use serde_json::{json, Value as JsonValue};
use std::error::Error;
use ureq::{http::Uri, Cookie};

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
                let max_age = ts.unix_timestamp() - CookieOffsetDateTime::now_utc().unix_timestamp();
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
        let uri = Uri::try_from(uri_str.as_str()).unwrap_or_else(|_| Uri::from_static("https://example.com"));

        if let Ok(cookie) = Cookie::parse(set_cookie, &uri) {
            let _ = jar.insert(cookie, &uri);
        }
    }
    jar.release();
}

pub fn build_flaresolverr_client(url: &str, flaresolverr_url: &str) -> Result<Agent, Box<dyn Error>> {
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


#[derive(Clone)]
pub struct FlareClient {
    pub agent: Agent,
    pub flaresolverr_url: Option<String>,
    pub session_id: Option<String>,
    // Headers suggested by FlareSolverr for this session (applied on each direct request)
    pub default_headers: Vec<(String, String)>,
}

impl FlareClient {
    pub fn from_env(origin_url: &str) -> Result<Self, Box<dyn Error>> {
        let flaresolverr_url = std::env::var("FLARESOLVERR_URL").ok();

        // If not configured, just return a plain agent.
        if flaresolverr_url.is_none() {
            return Ok(Self {
                agent: build_ureq_agent(None),
                flaresolverr_url: None,
                session_id: None,
                default_headers: vec![],
            });
        }
        let flaresolverr_url = flaresolverr_url.unwrap();

        // Optional: create a session if FLARESOLVERR_SESSION is set (or create one automatically)
        let mut session_id = std::env::var("FLARESOLVERR_SESSION").ok();
        if session_id.is_none() {
            // Try to create a session, but don't fail hard if it errors.
            if let Ok(mut resp) = ureq::post(&flaresolverr_url)
                .header("Content-Type", "application/json")
                .send_json(&json!({"cmd":"sessions.create"}))
            {
                if let Ok(text) = resp.body_mut().read_to_string() {
                    #[derive(serde::Deserialize)]
                    struct Created { status: String, session: Option<String> }
                    if let Ok(Created { status, session }) = serde_json::from_str(&text) {
                        if status == "ok" {
                            session_id = session;
                        }
                    }
                }
            }
        }

        // Initial solve for the origin (non-fatal fallback to plain agent)
        match solve_with_flaresolverr(&flaresolverr_url, origin_url, session_id.as_deref()) {
            Ok(solved) => {
                let agent = build_ureq_agent(Some(&solved.user_agent));
                insert_flaresolverr_cookies_into_agent(&agent, solved.cookies);
                Ok(Self {
                    agent,
                    flaresolverr_url: Some(flaresolverr_url),
                    session_id,
                    default_headers: solved.headers,
                })
            }
            Err(_) => Ok(Self {
                agent: build_ureq_agent(None),
                flaresolverr_url: Some(flaresolverr_url),
                session_id,
                default_headers: vec![],
            }),
        }
    }

    /// Fetch body as text, using either direct agent or FlareSolverr proxy mode.
    /// If direct mode returns 403/503, it re-solves once and retries.
    pub fn fetch_text(&mut self, url: &str) -> Result<String, Box<dyn Error>> {
        if let Some(ref _fs_url) = self.flaresolverr_url {
            // Try through FlareSolverr first
            match self.proxy_fetch_text(url) {
                Ok(text) => return Ok(text),
                Err(_e) => {
                    // Fallback to direct request on *any* FS failure
                    return self.direct_get(url);
                }
            }
        }

        // No FlareSolverr configured — direct request only
        self.direct_get(url)
    }

    fn proxy_fetch_text(&self, url: &str) -> Result<String, Box<dyn std::error::Error>> {
        let fs_url = self
            .flaresolverr_url
            .as_ref()
            .ok_or_else(|| "FLARESOLVERR_URL not set".to_string())?;
        let payload = if let Some(ref sid) = self.session_id {
            json!({"cmd":"request.get","url":url,"maxTimeout":60000,"session":sid})
        } else {
            json!({"cmd":"request.get","url":url,"maxTimeout":60000})
        };

        let mut resp = ureq::post(fs_url)
            .header("Content-Type", "application/json")
            .send_json(&payload)?;
        let text = resp.body_mut().read_to_string()?;
        let body: FlareSolverrResponse = serde_json::from_str(&text)?;
        if body.status != "ok" {
            return Err(format!("FlareSolverr error: {}", body.message).into());
        }
        Ok(body.solution.response)
    }


    fn direct_get(&self, url: &str) -> Result<String, Box<dyn std::error::Error>> {
        let req = self
            .default_headers
            .iter()
            .fold(self.agent.get(url), |req, (k, v)| req.header(k, v));

        let mut resp = req.call()?;
        let body = resp.body_mut().read_to_string()?;
        Ok(body)
    }
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
) -> Result<Solved, Box<dyn Error>> {
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
        return Err(format!("FlareSolverr error: {}", body.message).into());
    }

    // Extract headers as downcased strings (avoid collisions like "Set-Cookie")
    let mut hdrs: Vec<(String, String)> = vec![];
    if let Some(obj) = body.solution.headers.as_object() {
        for (k, v) in obj.iter() {
            if let Some(s) = v.as_str() {
                if !k.eq_ignore_ascii_case("set-cookie") {
                    hdrs.push((k.to_string(), s.to_string()));
                }
            } else {
                // keep JSON values stringified
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
    use std::env;
    use super::*;
    use serde_json::json;

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
