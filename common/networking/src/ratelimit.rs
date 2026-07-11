use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::trace;

#[derive(Debug)]
pub struct RateLimiter {
    interval: Duration,
    next_allowed: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(requests_per_second: f64) -> Option<Self> {
        if !(requests_per_second.is_finite()) || requests_per_second <= 0.0 {
            return None;
        }
        let secs = 1.0 / requests_per_second;
        let interval = Duration::from_secs_f64(secs.max(0.0));
        Some(Self {
            interval,
            next_allowed: Mutex::new(Instant::now()),
        })
    }

    pub fn acquire(&self) {
        loop {
            let now = Instant::now();
            let sleep_for = {
                let mut next = self.next_allowed.lock().unwrap();
                if now >= *next {
                    *next = now + self.interval;
                    None
                } else {
                    Some(*next - now)
                }
            };

            if let Some(dur) = sleep_for {
                trace!("Rate limiter sleeping for {:?}", dur);
                std::thread::sleep(dur);
            } else {
                return;
            }
        }
    }
}
