//! Signing in: password rules and hashing, sessions for the web page, the
//! end-to-end key exchange, and the guard that slows down and then locks
//! out anyone guessing passwords.
//!
//! A password is never stored as is. Before Argon2id (64 MiB, 3 passes)
//! hashes it, it is mixed with a random 32-byte "pepper" kept encrypted for
//! this Windows account (DPAPI). A copied settings file alone is therefore
//! useless for guessing passwords offline: without the pepper there is
//! nothing to test guesses against.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use ring::rand::SecureRandom;

/// Hashes made with the pepper start with this.
const PEPPERED: &str = "feather2$";
/// Rounds of PBKDF2 for the end-to-end key, in the browser and here.
pub const E2E_ROUNDS: u32 = 600_000;

fn argon2() -> Argon2<'static> {
    let params = Params::new(64 * 1024, 3, 1, Some(32)).expect("valid Argon2 settings");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Only one hash is checked at a time, so a flood of sign-ins can't use
/// 64 MiB each.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn random(bytes: &mut [u8]) {
    ring::rand::SystemRandom::new()
        .fill(bytes)
        .expect("the system random source failed");
}

fn pepper_path() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(
            std::env::temp_dir()
                .join("feather-test-sharing")
                .join("pepper.key"),
        );
    }
    super::tls::folder().map(|folder| folder.join("pepper.key"))
}

/// The pepper, made on first use and kept encrypted for this account.
fn pepper() -> Result<&'static [u8; 32], String> {
    static PEPPER: OnceLock<[u8; 32]> = OnceLock::new();
    if let Some(pepper) = PEPPER.get() {
        return Ok(pepper);
    }
    let path = pepper_path().ok_or("Could not find Feather's folder.")?;
    let loaded = std::fs::read(&path)
        .ok()
        .and_then(|sealed| super::secret::unprotect(&sealed).ok())
        .and_then(|plain| <[u8; 32]>::try_from(plain.as_slice()).ok());
    let pepper = match loaded {
        Some(pepper) => pepper,
        None => {
            let mut fresh = [0u8; 32];
            random(&mut fresh);
            if let Some(folder) = path.parent() {
                std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
            }
            std::fs::write(&path, super::secret::protect(&fresh)?)
                .map_err(|error| format!("Could not save the password key: {error}"))?;
            fresh
        }
    };
    Ok(PEPPER.get_or_init(|| pepper))
}

/// The password mixed with the pepper, as Argon2 sees it.
fn peppered(password: &str) -> Result<Vec<u8>, String> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, pepper()?);
    Ok(ring::hmac::sign(&key, password.as_bytes())
        .as_ref()
        .to_vec())
}

/// Hashes a new password for the settings file.
pub fn hash_password(password: &str) -> Result<String, String> {
    if password.is_empty() {
        return Err("There's no password to protect.".into());
    }
    let mut salt = [0u8; 16];
    random(&mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|error| error.to_string())?;
    let input = peppered(password)?;
    let _turn = ONE_AT_A_TIME.lock().unwrap();
    argon2()
        .hash_password(&input, &salt)
        .map(|hash| format!("{PEPPERED}{hash}"))
        .map_err(|error| format!("Could not protect the password: {error}"))
}

/// Checks a user name and password. The hash is always checked, so a wrong
/// user name takes as long as a wrong password.
pub fn check(user: &str, password: &str, expected_user: &str, hash: &str) -> bool {
    let (input, phc) = match hash.strip_prefix(PEPPERED) {
        Some(phc) => match peppered(password) {
            Ok(input) => (input, phc),
            Err(_) => return false,
        },
        // Hashes from before the pepper still work.
        None => (password.as_bytes().to_vec(), hash),
    };
    let password_ok = {
        let _turn = ONE_AT_A_TIME.lock().unwrap();
        PasswordHash::new(phc)
            .map(|parsed| argon2().verify_password(&input, &parsed).is_ok())
            .unwrap_or(false)
    };
    let user_ok = same(user.as_bytes(), expected_user.as_bytes());
    password_ok && user_ok && !expected_user.is_empty()
}

/// Compares without stopping at the first difference.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    let digest = |bytes: &[u8]| ring::digest::digest(&ring::digest::SHA256, bytes);
    let (a, b) = (digest(a), digest(b));
    a.as_ref()
        .iter()
        .zip(b.as_ref())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// A random token as hex, from the system's secure random source.
pub fn token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    random(&mut buffer);
    hex(&buffer)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buffer = [0u8; N];
    random(&mut buffer);
    buffer
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

// ----- End-to-end ---------------------------------------------------------

/// The key both sides derive from the password: PBKDF2-SHA256, the same in
/// the browser's Web Crypto.
pub fn e2e_key(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        std::num::NonZeroU32::new(E2E_ROUNDS).unwrap(),
        salt,
        password.as_bytes(),
        &mut key,
    );
    key
}

/// The salt for the end-to-end key: fixed for this computer, and not
/// guessable from outside because it comes from the pepper.
pub fn e2e_salt() -> [u8; 16] {
    let pepper = pepper().copied().unwrap_or([0u8; 32]);
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&proof(&pepper, "feather e2e salt", &[])[..16]);
    salt
}

/// HMAC-SHA256 over `parts` joined, labelled so a proof for one purpose
/// can't stand in for another.
pub fn proof(key: &[u8], label: &str, parts: &[&[u8]]) -> Vec<u8> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let mut context = ring::hmac::Context::with_key(&key);
    context.update(label.as_bytes());
    for part in parts {
        context.update(part);
    }
    context.sign().as_ref().to_vec()
}

/// The session's encryption key: HKDF-SHA256 from the password key and
/// both sides' fresh nonces, so every sign-in gets its own.
pub fn session_key(key: &[u8], server: &[u8], client: &[u8]) -> [u8; 32] {
    struct Length;
    impl ring::hkdf::KeyType for Length {
        fn len(&self) -> usize {
            32
        }
    }
    let mut salt_bytes = server.to_vec();
    salt_bytes.extend_from_slice(client);
    let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &salt_bytes);
    let info: &[&[u8]] = &[b"feather session"];
    let mut out = [0u8; 32];
    salt.extract(key)
        .expand(info, Length)
        .and_then(|okm| okm.fill(&mut out))
        .expect("HKDF output");
    out
}

/// Sign-ins started with `/e2e/hello` and not finished yet.
#[derive(Default)]
pub struct Handshakes {
    pending: Mutex<HashMap<String, (Vec<u8>, Instant)>>,
}

impl Handshakes {
    /// Starts one: its id and the server's nonce.
    pub fn start(&self) -> (String, Vec<u8>) {
        let nonce = random_bytes::<16>().to_vec();
        let id = token(16);
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(120));
        if pending.len() >= 256 {
            pending.clear();
        }
        pending.insert(id.clone(), (nonce.clone(), Instant::now()));
        (id, nonce)
    }

    /// Takes the nonce of a pending sign-in; each can be used once.
    pub fn take(&self, id: &str) -> Option<Vec<u8>> {
        let (nonce, at) = self.pending.lock().unwrap().remove(id)?;
        (at.elapsed() < Duration::from_secs(120)).then_some(nonce)
    }
}

// ----- Lockout ------------------------------------------------------------

/// Wrong passwords before an address is locked out.
const FREE_TRIES: u32 = 5;
/// The first lockout; each further miss doubles it.
const FIRST_LOCK: Duration = Duration::from_secs(30);
const LONGEST_LOCK: Duration = Duration::from_secs(60 * 60);
/// Misses are forgotten this long after the last one.
const FORGET: Duration = Duration::from_secs(60 * 60);
/// Misses from everywhere together that lock everyone out for a while.
const GLOBAL_TRIES: usize = 60;
const GLOBAL_WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
struct Strikes {
    misses: u32,
    last: Option<Instant>,
    until: Option<Instant>,
}

#[derive(Default)]
struct GuardState {
    by_address: HashMap<IpAddr, Strikes>,
    recent: Vec<Instant>,
}

/// Remembers wrong passwords per address, shared by the web and FTP
/// servers.
#[derive(Default)]
pub struct Guard {
    state: Mutex<GuardState>,
}

impl Guard {
    /// `Err(wait)` while `address` (or everyone) is locked out.
    pub fn check(&self, address: IpAddr) -> Result<(), Duration> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state
            .recent
            .retain(|at| now.duration_since(*at) < GLOBAL_WINDOW);
        if state.recent.len() >= GLOBAL_TRIES {
            let oldest = state.recent[0];
            return Err(GLOBAL_WINDOW.saturating_sub(now.duration_since(oldest)));
        }
        match state
            .by_address
            .get(&address)
            .and_then(|strikes| strikes.until)
        {
            Some(until) if until > now => Err(until - now),
            _ => Ok(()),
        }
    }

    pub fn failed(&self, address: IpAddr) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.recent.push(now);
        if state.by_address.len() > 4096 {
            state.by_address.retain(|_, strikes| {
                strikes
                    .last
                    .is_some_and(|last| now.duration_since(last) < FORGET)
            });
        }
        let strikes = state.by_address.entry(address).or_default();
        if strikes
            .last
            .is_some_and(|last| now.duration_since(last) >= FORGET)
        {
            *strikes = Strikes::default();
        }
        strikes.misses += 1;
        strikes.last = Some(now);
        if strikes.misses >= FREE_TRIES {
            let doublings = (strikes.misses - FREE_TRIES).min(10);
            let lock = (FIRST_LOCK * 2u32.pow(doublings)).min(LONGEST_LOCK);
            strikes.until = Some(now + lock);
        }
    }

    pub fn succeeded(&self, address: IpAddr) {
        self.state.lock().unwrap().by_address.remove(&address);
    }

    /// How long to wait before answering a wrong password: longer with
    /// each miss, so guessing slows down before the lockout starts.
    pub fn delay(&self, address: IpAddr) -> Duration {
        let state = self.state.lock().unwrap();
        let misses = state
            .by_address
            .get(&address)
            .map(|strikes| strikes.misses)
            .unwrap_or(0);
        Duration::from_millis(400 * u64::from(misses.clamp(1, 8)))
    }
}

// ----- Sessions -----------------------------------------------------------

/// How long a web sign-in lasts without use, and at most.
const IDLE: Duration = Duration::from_secs(2 * 60 * 60);
const LONGEST: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_SESSIONS: usize = 128;
/// Nonces remembered per session, so a captured request can't be replayed.
const REMEMBERED_NONCES: usize = 4096;

/// What a session knows.
#[derive(Clone)]
pub struct Session {
    pub csrf: String,
    /// The end-to-end key, for sessions that signed in that way.
    pub key: Option<[u8; 32]>,
}

struct Live {
    session: Session,
    created: Instant,
    seen: Instant,
    nonces: HashSet<[u8; 12]>,
    order: VecDeque<[u8; 12]>,
}

/// Signed-in browsers, by the random token in their cookie.
#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Live>>,
}

impl Sessions {
    /// Starts a session. Returns its cookie token and its form token.
    pub fn create(&self, key: Option<[u8; 32]>) -> (String, String) {
        let now = Instant::now();
        let (token_value, csrf) = (token(32), token(24));
        let mut map = self.map.lock().unwrap();
        map.retain(|_, live| alive(live, now));
        if map.len() >= MAX_SESSIONS {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, live)| live.seen)
                .map(|(key, _)| key.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(
            token_value.clone(),
            Live {
                session: Session {
                    csrf: csrf.clone(),
                    key,
                },
                created: now,
                seen: now,
                nonces: HashSet::new(),
                order: VecDeque::new(),
            },
        );
        (token_value, csrf)
    }

    /// A live session, marking it used.
    pub fn get(&self, token_value: &str) -> Option<Session> {
        let now = Instant::now();
        let mut map = self.map.lock().unwrap();
        let live = map.get_mut(token_value)?;
        if !alive(live, now) {
            map.remove(token_value);
            return None;
        }
        live.seen = now;
        Some(live.session.clone())
    }

    /// Records a request nonce; false when it was seen before (a replay).
    pub fn fresh_nonce(&self, token_value: &str, nonce: [u8; 12]) -> bool {
        let mut map = self.map.lock().unwrap();
        let Some(live) = map.get_mut(token_value) else {
            return false;
        };
        if !live.nonces.insert(nonce) {
            return false;
        }
        live.order.push_back(nonce);
        if live.order.len() > REMEMBERED_NONCES {
            if let Some(old) = live.order.pop_front() {
                live.nonces.remove(&old);
            }
        }
        true
    }

    pub fn remove(&self, token_value: &str) {
        self.map.lock().unwrap().remove(token_value);
    }
}

fn alive(live: &Live, now: Instant) -> bool {
    now.duration_since(live.seen) < IDLE && now.duration_since(live.created) < LONGEST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_hash_with_the_pepper_and_check() {
        assert!(hash_password("").is_err());
        assert!(hash_password("a").is_ok(), "any password will do");
        let hash = hash_password("abcd").expect("hash");
        assert!(hash.starts_with("feather2$$argon2id$v=19$m=65536,t=3,p=1$"));
        assert!(check("me", "abcd", "me", &hash));
        assert!(!check("me", "abce", "me", &hash));
        assert!(!check("you", "abcd", "me", &hash));
        assert!(!check("me", "anything", "me", ""));
    }

    #[test]
    fn hashes_from_before_the_pepper_still_work() {
        let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
        let legacy = Argon2::default()
            .hash_password(b"old password", &salt)
            .unwrap()
            .to_string();
        assert!(check("me", "old password", "me", &legacy));
        assert!(!check("me", "new password", "me", &legacy));
    }

    #[test]
    fn repeated_misses_lock_an_address() {
        let guard = Guard::default();
        let address: IpAddr = "192.168.1.9".parse().unwrap();
        for _ in 0..FREE_TRIES - 1 {
            guard.failed(address);
            assert!(guard.check(address).is_ok());
        }
        assert!(guard.delay(address) > Duration::from_millis(400));
        guard.failed(address);
        assert!(guard.check(address).is_err());
        assert!(guard.check("192.168.1.10".parse().unwrap()).is_ok());
        guard.succeeded(address);
        assert!(guard.check(address).is_ok());
    }

    #[test]
    fn sessions_end_and_refuse_replays() {
        let sessions = Sessions::default();
        let (key, csrf) = sessions.create(Some([7; 32]));
        assert_eq!(key.len(), 64);
        let session = sessions.get(&key).unwrap();
        assert_eq!(session.csrf, csrf);
        assert_eq!(session.key, Some([7; 32]));
        assert!(sessions.fresh_nonce(&key, [1; 12]));
        assert!(!sessions.fresh_nonce(&key, [1; 12]));
        sessions.remove(&key);
        assert!(sessions.get(&key).is_none());
    }

    #[test]
    fn the_key_exchange_agrees() {
        let key = e2e_key("abcd", b"salt-salt-salt-1");
        assert_eq!(key, e2e_key("abcd", b"salt-salt-salt-1"));
        assert_ne!(key, e2e_key("abce", b"salt-salt-salt-1"));
        let a = session_key(&key, b"server", b"client");
        assert_eq!(a, session_key(&key, b"server", b"client"));
        assert_ne!(a, session_key(&key, b"server", b"other"));
        assert_eq!(proof(&key, "x", &[b"1", b"2"]), proof(&key, "x", &[b"12"]));
        assert_eq!(unhex(&hex(&[1, 255, 16])), Some(vec![1, 255, 16]));
    }
}
