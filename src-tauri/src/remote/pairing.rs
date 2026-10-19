//! One-use, short-lived pairing codes. Nothing in this module is persisted.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::rand::{ct_eq, random_b64};

pub const CODE_TTL: Duration = Duration::from_secs(300);
pub const MAX_FAILURES: usize = 5;
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    Invalid,
    Expired,
    RateLimited,
}

#[derive(Default)]
pub struct Pairing {
    codes: Mutex<Vec<(String, Instant)>>,
    failures: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl Pairing {
    pub fn start(&self, now: Instant) -> Result<String, String> {
        let code = random_b64(16)?;
        let mut codes = self.codes.lock().unwrap_or_else(PoisonError::into_inner);
        codes.clear();
        // The dialog owns the returned code; redemption keeps its own copy.
        codes.push((code.clone(), now));
        drop(codes);
        Ok(code)
    }

    pub fn cancel(&self) {
        self.codes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    pub fn redeem(&self, code: &str, peer: IpAddr, now: Instant) -> Result<(), PairError> {
        // Keep admission and redemption together so concurrent failures cannot
        // pass the limit and concurrent successes cannot reuse a code.
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        failures.retain(|_, attempts| {
            attempts.retain(|at| now.saturating_duration_since(*at) < FAILURE_WINDOW);
            !attempts.is_empty()
        });
        if failures.get(&peer).is_some_and(|v| v.len() >= MAX_FAILURES) {
            return Err(PairError::RateLimited);
        }
        let mut codes = self.codes.lock().unwrap_or_else(PoisonError::into_inner);
        let matched = codes
            .iter()
            .position(|(candidate, _)| ct_eq(candidate.as_bytes(), code.as_bytes()));
        let candidate = matched.map(|index| codes.remove(index));
        drop(codes);
        let result = match candidate {
            Some((_, created)) if now.saturating_duration_since(created) < CODE_TTL => Ok(()),
            Some(_) => Err(PairError::Expired),
            None => Err(PairError::Invalid),
        };
        if result.is_err() {
            failures.entry(peer).or_default().push(now);
        }
        drop(failures);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::time::{Duration, Instant};

    #[test]
    fn a_code_is_redeemable_once_and_expires() {
        let p = Pairing::default();
        let t0 = Instant::now();
        let code = p.start(t0).unwrap();
        let ip: IpAddr = "192.168.1.50".parse().unwrap();
        assert!(p.redeem(&code, ip, t0).is_ok());
        assert!(matches!(p.redeem(&code, ip, t0), Err(PairError::Invalid)));
        let code2 = p.start(t0).unwrap();
        assert!(matches!(
            p.redeem(&code2, ip, t0 + Duration::from_secs(301)),
            Err(PairError::Expired)
        ));
    }

    #[test]
    fn five_failures_per_minute_rate_limit_the_peer_only() {
        let p = Pairing::default();
        let t0 = Instant::now();
        let code = p.start(t0).unwrap();
        let bad: IpAddr = "192.168.1.66".parse().unwrap();
        for _ in 0..5 {
            assert!(matches!(p.redeem("nope", bad, t0), Err(PairError::Invalid)));
        }
        assert!(
            matches!(p.redeem(&code, bad, t0), Err(PairError::RateLimited)),
            "even the right code"
        );
        let good: IpAddr = "192.168.1.50".parse().unwrap();
        assert!(p.redeem(&code, good, t0).is_ok());
        assert!(p.start(t0 + Duration::from_secs(61)).is_ok());
    }

    #[test]
    fn replacement_cancel_and_expiry_boundary() {
        let p = Pairing::default();
        let now = Instant::now();
        let ip = "192.168.1.50".parse().unwrap();
        let old = p.start(now).unwrap();
        let code = p.start(now).unwrap();
        assert_eq!(code.len(), 22);
        assert!(matches!(p.redeem(&old, ip, now), Err(PairError::Invalid)));
        p.cancel();
        assert!(matches!(p.redeem(&code, ip, now), Err(PairError::Invalid)));
        let code = p.start(now).unwrap();
        assert!(matches!(
            p.redeem(&code, ip, now + Duration::from_secs(300)),
            Err(PairError::Expired)
        ));
        assert!(matches!(
            p.redeem(&code, ip, now + Duration::from_secs(301)),
            Err(PairError::Invalid)
        ));
    }

    #[test]
    fn failure_window_expires_without_resetting_on_start_or_cancel() {
        let p = Pairing::default();
        let now = Instant::now();
        let ip = "192.168.1.50".parse().unwrap();
        for _ in 0..5 {
            assert!(matches!(
                p.redeem("wrong", ip, now),
                Err(PairError::Invalid)
            ));
        }
        p.cancel();
        let code = p.start(now).unwrap();
        assert!(matches!(
            p.redeem(&code, ip, now + Duration::from_secs(59)),
            Err(PairError::RateLimited)
        ));
        assert!(p.redeem(&code, ip, now + Duration::from_secs(60)).is_ok());
    }
}
