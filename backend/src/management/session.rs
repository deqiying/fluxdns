//! 仅存于进程内存的不透明 WebUI session。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cookie::{Cookie, SameSite};
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub(crate) const SESSION_ABSOLUTE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
pub(crate) const SESSION_GLOBAL_CAPACITY: usize = 4096;
pub(crate) const SESSION_PER_USER_CAPACITY: usize = 16;
const HTTPS_COOKIE_NAME: &str = "__Host-fluxdns_session";
const HTTP_COOKIE_NAME: &str = "fluxdns_session";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SessionView {
    pub(crate) user: SessionUserView,
    pub(crate) expires_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SessionUserView {
    pub(crate) name: String,
}

struct SessionRecord {
    username: String,
    created_at: Instant,
    last_seen: Instant,
    expires_at: Instant,
    expires_at_utc: SystemTime,
}

pub(crate) struct IssuedSession {
    pub(crate) token: String,
    pub(crate) view: SessionView,
}

/// 有界、进程内 session store；进程重启即全部失效。
pub(crate) struct SessionStore {
    sessions: Mutex<HashMap<String, SessionRecord>>,
    secure: bool,
}

impl SessionStore {
    pub(crate) fn new(secure: bool) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            secure,
        }
    }

    pub(crate) fn issue(&self, username: String) -> Result<IssuedSession, SessionError> {
        let now = Instant::now();
        let now_utc = SystemTime::now();
        let expires_at = now + SESSION_ABSOLUTE_TTL;
        let expires_at_utc = now_utc + SESSION_ABSOLUTE_TTL;
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| SessionError::RandomSource)?;
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?;
        purge_expired(&mut sessions, now);
        enforce_capacity(&mut sessions, &username);
        sessions.insert(
            token.clone(),
            SessionRecord {
                username: username.clone(),
                created_at: now,
                last_seen: now,
                expires_at,
                expires_at_utc,
            },
        );
        Ok(IssuedSession {
            token,
            view: session_view(username, expires_at_utc)?,
        })
    }

    pub(crate) fn lookup(&self, token: &str) -> Result<Option<SessionView>, SessionError> {
        let now = Instant::now();
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?;
        purge_expired(&mut sessions, now);
        let Some(record) = sessions.get_mut(token) else {
            return Ok(None);
        };
        if now.duration_since(record.last_seen) >= SESSION_IDLE_TTL {
            sessions.remove(token);
            return Ok(None);
        }
        record.last_seen = now;
        session_view(record.username.clone(), record.expires_at_utc).map(Some)
    }

    pub(crate) fn revoke(&self, token: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(token);
        }
    }

    pub(crate) fn revoke_all(&self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.clear();
        }
    }

    pub(crate) fn cookie_name(&self) -> &'static str {
        if self.secure {
            HTTPS_COOKIE_NAME
        } else {
            HTTP_COOKIE_NAME
        }
    }

    pub(crate) fn set_cookie(&self, token: String) -> String {
        Cookie::build((self.cookie_name(), token))
            .http_only(true)
            .same_site(SameSite::Strict)
            .secure(self.secure)
            .path("/")
            .build()
            .to_string()
    }

    pub(crate) fn clear_cookie(&self) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        format!(
            "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}",
            self.cookie_name()
        )
    }

    pub(crate) fn token_from_header(&self, header: &str) -> Option<String> {
        Cookie::split_parse(header)
            .filter_map(Result::ok)
            .find(|cookie| cookie.name() == self.cookie_name())
            .map(|cookie| cookie.value().to_owned())
    }
}

fn purge_expired(sessions: &mut HashMap<String, SessionRecord>, now: Instant) {
    sessions.retain(|_, session| {
        now < session.expires_at && now.duration_since(session.last_seen) < SESSION_IDLE_TTL
    });
}

fn enforce_capacity(sessions: &mut HashMap<String, SessionRecord>, username: &str) {
    while sessions.len() >= SESSION_GLOBAL_CAPACITY
        || sessions
            .values()
            .filter(|session| session.username == username)
            .count()
            >= SESSION_PER_USER_CAPACITY
    {
        let candidate = sessions
            .iter()
            .filter(|(_, session)| {
                sessions.len() >= SESSION_GLOBAL_CAPACITY || session.username == username
            })
            .min_by_key(|(_, session)| session.created_at)
            .map(|(token, _)| token.clone());
        let Some(token) = candidate else {
            break;
        };
        sessions.remove(&token);
    }
}

fn session_view(username: String, expires_at: SystemTime) -> Result<SessionView, SessionError> {
    let expires_at = OffsetDateTime::from(expires_at)
        .format(&Rfc3339)
        .map_err(|_| SessionError::TimeFormat)?;
    Ok(SessionView {
        user: SessionUserView { name: username },
        expires_at,
    })
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionError {
    #[error("session random source failed")]
    RandomSource,
    #[error("session store is unavailable")]
    Unavailable,
    #[error("session expiry formatting failed")]
    TimeFormat,
}

#[cfg(test)]
mod tests {
    use super::SessionStore;

    #[test]
    fn cookie_policy_tracks_public_origin_transport() {
        let secure = SessionStore::new(true);
        let header = secure.set_cookie("token".to_owned());
        assert!(header.starts_with("__Host-fluxdns_session="));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Strict"));
        assert!(header.contains("Secure"));
        assert!(!header.contains("Domain="));

        let http = SessionStore::new(false);
        let header = http.set_cookie("token".to_owned());
        assert!(header.starts_with("fluxdns_session="));
        assert!(!header.contains("; Secure"));
    }

    #[test]
    fn issued_session_is_opaque_and_revocable() {
        let store = SessionStore::new(false);
        let issued = store.issue("admin".to_owned()).unwrap();
        assert!(issued.token.len() >= 43);
        assert!(store.lookup(&issued.token).unwrap().is_some());
        store.revoke(&issued.token);
        assert!(store.lookup(&issued.token).unwrap().is_none());
    }
}
