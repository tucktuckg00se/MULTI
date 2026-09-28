//! Web GUI sign-in: argon2id password hashes, server-side sessions and a
//! per-IP limit on failed sign-ins. Time comes from an injectable [`Clock`] so
//! expiry and back-off can be tested.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use sha2::{Digest, Sha256};

/// Minimum password length, in characters.
pub const MIN_PASSWORD_CHARS: usize = 8;
/// A session ends after this long without a request.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(12 * 3600);
/// A session ends this long after sign-in, whatever happens.
pub const ABSOLUTE_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);
/// Failed sign-ins from one address before back-off starts.
pub const FREE_FAILURES: u32 = 5;
/// First back-off; doubles with each further failure, up to [`MAX_BACKOFF`].
pub const FIRST_BACKOFF: Duration = Duration::from_secs(30);
pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Monotonic time since an arbitrary start.
pub type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

pub fn system_clock() -> Clock {
    let start = Instant::now();
    Arc::new(move || start.elapsed())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Constant-time comparison (length is not secret).
pub fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| anyhow!("no system randomness: {e}"))?;
    Ok(b)
}

// ---------------------------------------------------------------- passwords

/// Rejects passwords that are too short.
pub fn check_new_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        bail!("the password needs at least {MIN_PASSWORD_CHARS} characters");
    }
    Ok(())
}

/// Argon2id (default parameters: 19 MiB, 2 passes) PHC string.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::encode_b64(&random_bytes::<16>()?).map_err(|e| anyhow!("salt: {e}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("cannot hash the password: {e}"))
}

/// Whether `password` matches the PHC string `hash`. A malformed hash never matches.
pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// `multi passwd`: stores the hash of `password` (and `username`, if given) in
/// the config at `path`, creating the file with defaults if missing.
pub fn set_password(path: &Path, username: Option<&str>, password: &str) -> Result<()> {
    check_new_password(password)?;
    let mut config = crate::web::load_or_create(path)?;
    if let Some(u) = username {
        if u.trim().is_empty() {
            bail!("the user name cannot be empty");
        }
        config.web.username = u.trim().to_string();
    }
    config.web.password_hash = Some(hash_password(password)?);
    crate::web::write_atomic(path, &config.to_toml()?)
        .map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------- sessions

#[derive(Clone, Copy)]
struct Session {
    created: Duration,
    last: Duration,
}

/// Signed-in browsers, in memory (a restart signs everyone out). Keyed by the
/// SHA-256 of the cookie value, so lookups don't compare secrets.
pub struct Sessions {
    clock: Clock,
    map: Mutex<HashMap<String, Session>>,
}

fn session_key(id: &str) -> String {
    hex(&Sha256::digest(id.as_bytes()))
}

impl Sessions {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// A new session; returns the cookie value (256 random bits, hex).
    pub fn create(&self) -> Result<String> {
        let id = hex(&random_bytes::<32>()?);
        let now = (self.clock)();
        let mut map = lock(&self.map);
        map.retain(|_, s| alive(s, now));
        map.insert(
            session_key(&id),
            Session {
                created: now,
                last: now,
            },
        );
        Ok(id)
    }

    /// Whether `id` is a live session; refreshes its idle timer.
    pub fn check(&self, id: &str) -> bool {
        let now = (self.clock)();
        let key = session_key(id);
        let mut map = lock(&self.map);
        match map.get_mut(&key) {
            Some(s) if alive(s, now) => {
                s.last = now;
                true
            }
            Some(_) => {
                map.remove(&key);
                false
            }
            None => false,
        }
    }

    pub fn remove(&self, id: &str) {
        lock(&self.map).remove(&session_key(id));
    }

    /// Ends every session (after a password change).
    pub fn clear(&self) {
        lock(&self.map).clear();
    }
}

fn alive(s: &Session, now: Duration) -> bool {
    now.saturating_sub(s.last) < IDLE_TIMEOUT && now.saturating_sub(s.created) < ABSOLUTE_TIMEOUT
}

// ---------------------------------------------------------------- rate limit

#[derive(Clone, Copy)]
struct Failures {
    count: u32,
    last: Duration,
    until: Duration,
}

/// Failed sign-ins per client address: after [`FREE_FAILURES`], each failure
/// locks the address out for [`FIRST_BACKOFF`] doubled per extra failure, at
/// most [`MAX_BACKOFF`]. Forgotten a day after the last failure.
pub struct Limiter {
    clock: Clock,
    map: Mutex<HashMap<IpAddr, Failures>>,
}

const FORGET_AFTER: Duration = Duration::from_secs(24 * 3600);

impl Limiter {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Time left before `ip` may try again, if it is locked out.
    pub fn blocked(&self, ip: IpAddr) -> Option<Duration> {
        let now = (self.clock)();
        lock(&self.map)
            .get(&ip)
            .and_then(|f| f.until.checked_sub(now))
            .filter(|d| !d.is_zero())
    }

    /// Records a failure; returns the lock-out it starts, if any.
    pub fn fail(&self, ip: IpAddr) -> Option<Duration> {
        let now = (self.clock)();
        let mut map = lock(&self.map);
        map.retain(|_, f| now.saturating_sub(f.last) < FORGET_AFTER);
        let f = map.entry(ip).or_insert(Failures {
            count: 0,
            last: now,
            until: now,
        });
        f.count = f.count.saturating_add(1);
        f.last = now;
        if f.count < FREE_FAILURES {
            return None;
        }
        let doublings = (f.count - FREE_FAILURES).min(16);
        let wait = FIRST_BACKOFF
            .saturating_mul(1u32 << doublings)
            .min(MAX_BACKOFF);
        f.until = now + wait;
        Some(wait)
    }

    pub fn clear(&self, ip: IpAddr) {
        lock(&self.map).remove(&ip);
    }
}

/// "3 min" / "40 s" for messages.
pub fn human(d: Duration) -> String {
    let s = d.as_secs().max(1);
    if s >= 60 {
        format!("{} min", s.div_ceil(60))
    } else {
        format!("{s} s")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fake_clock() -> (Clock, Arc<AtomicU64>) {
        let t = Arc::new(AtomicU64::new(0));
        let c = t.clone();
        (
            Arc::new(move || Duration::from_secs(c.load(Ordering::SeqCst))),
            t,
        )
    }

    #[test]
    fn hash_and_verify() {
        let h = hash_password("correct horse").unwrap();
        assert!(h.starts_with("$argon2id$"), "{h}");
        assert!(!h.contains("correct horse"));
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("correct hors", &h));
        assert!(!verify_password("correct horse", "not a hash"));
        // Salted: two hashes of one password differ.
        assert_ne!(h, hash_password("correct horse").unwrap());
        assert!(check_new_password("short").is_err());
        assert!(check_new_password("12345678").is_ok());
    }

    #[test]
    fn sessions_expire_when_idle_and_absolutely() {
        let (clock, t) = fake_clock();
        let s = Sessions::new(clock);
        let id = s.create().unwrap();
        assert_eq!(id.len(), 64);
        assert!(s.check(&id));
        assert!(!s.check("nope"));
        // Idle: 11 h is fine, then 12 h without a request ends it.
        t.store(11 * 3600, Ordering::SeqCst);
        assert!(s.check(&id));
        t.store(23 * 3600, Ordering::SeqCst);
        assert!(!s.check(&id));
        // Absolute: used every 10 h, it still ends after 7 days.
        t.store(0, Ordering::SeqCst);
        let id = s.create().unwrap();
        let mut h = 0;
        while h + 10 < 7 * 24 {
            h += 10;
            t.store(h * 3600, Ordering::SeqCst);
            assert!(s.check(&id), "at {h} h");
        }
        t.store(7 * 24 * 3600, Ordering::SeqCst);
        assert!(!s.check(&id));
        let id = s.create().unwrap();
        s.remove(&id);
        assert!(!s.check(&id));
    }

    #[test]
    fn failed_sign_ins_back_off_exponentially() {
        let (clock, t) = fake_clock();
        let l = Limiter::new(clock);
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let other: IpAddr = "10.0.0.10".parse().unwrap();
        for _ in 0..4 {
            assert_eq!(l.fail(ip), None);
            assert_eq!(l.blocked(ip), None);
        }
        assert_eq!(l.fail(ip), Some(Duration::from_secs(30)));
        assert_eq!(l.blocked(ip), Some(Duration::from_secs(30)));
        assert_eq!(l.blocked(other), None);
        t.store(30, Ordering::SeqCst);
        assert_eq!(l.blocked(ip), None);
        assert_eq!(l.fail(ip), Some(Duration::from_secs(60)));
        assert_eq!(l.fail(ip), Some(Duration::from_secs(120)));
        for _ in 0..10 {
            l.fail(ip);
        }
        assert_eq!(l.blocked(ip), Some(MAX_BACKOFF));
        l.clear(ip);
        assert_eq!(l.blocked(ip), None);
        assert_eq!(human(Duration::from_secs(900)), "15 min");
        assert_eq!(human(Duration::from_secs(30)), "30 s");
    }
}
