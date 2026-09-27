//! 仅存于进程内存的不透明 WebUI session。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cookie::{Cookie, SameSite};
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::contract::WebSocketTicket;

pub(crate) const SESSION_ABSOLUTE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
pub(crate) const SESSION_GLOBAL_CAPACITY: usize = 4096;
pub(crate) const SESSION_PER_USER_CAPACITY: usize = 16;
pub(crate) const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
pub(crate) const ACCESS_RENEW_WINDOW: Duration = Duration::from_secs(30);
pub(crate) const WS_TICKET_TTL: Duration = Duration::from_secs(30);
pub(crate) const WS_TICKET_GLOBAL_CAPACITY: usize = 128;
pub(crate) const WS_TICKET_PER_SESSION_CAPACITY: usize = 4;
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
    access_token: String,
    access_expires_at: Instant,
    access_expires_at_utc: SystemTime,
}

/// 仅登录/初始化/刷新响应携带访问凭据，普通 session 投影不包含 token。
#[derive(Serialize)]
pub(crate) struct AuthSession {
    pub(crate) session: SessionView,
    pub(crate) access_token: String,
    pub(crate) token_type: &'static str,
    pub(crate) access_expires_at_ms: u64,
}

pub(crate) struct IssuedSession {
    /// 仅用于 HttpOnly Cookie，不得作为 Bearer 或响应正文返回。
    pub(crate) token: String,
    pub(crate) view: AuthSession,
}

struct AccessRecord {
    session_token: String,
    expires_at: Instant,
}

struct WebSocketTicketRecord {
    session_token: String,
    expires_at: Instant,
}

#[derive(Default)]
struct SessionData {
    sessions: HashMap<String, SessionRecord>,
    accesses: HashMap<String, AccessRecord>,
    websocket_tickets: HashMap<String, WebSocketTicketRecord>,
}

/// 只在服务端连接 owner 内流转的 session 主键，不是可发送给浏览器的凭据。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct WebSocketSession(String);

impl WebSocketSession {
    pub(super) fn key(&self) -> &str {
        &self.0
    }
}

/// 有界、进程内 session store；进程重启即全部失效。
pub(crate) struct SessionStore {
    sessions: Mutex<SessionData>,
    secure: bool,
    changes: tokio::sync::watch::Sender<u64>,
}

impl SessionStore {
    pub(crate) fn new(secure: bool) -> Self {
        let (changes, _) = tokio::sync::watch::channel(0);
        Self {
            sessions: Mutex::new(SessionData::default()),
            secure,
            changes,
        }
    }

    pub(crate) fn issue(&self, username: String) -> Result<IssuedSession, SessionError> {
        let now = Instant::now();
        let now_utc = SystemTime::now();
        let expires_at = now + SESSION_ABSOLUTE_TTL;
        let expires_at_utc = now_utc + SESSION_ABSOLUTE_TTL;
        let token = random_token()?;
        let access_token = random_token()?;
        let mut data = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?;
        if token == access_token
            || data.sessions.contains_key(&token)
            || data.accesses.contains_key(&access_token)
            || data.sessions.contains_key(&access_token)
            || data.accesses.contains_key(&token)
        {
            return Err(SessionError::RandomSource);
        }
        let record = SessionRecord {
            username: username.clone(),
            created_at: now,
            last_seen: now,
            expires_at,
            expires_at_utc,
            access_token: access_token.clone(),
            access_expires_at: now + ACCESS_TOKEN_TTL,
            access_expires_at_utc: now_utc + ACCESS_TOKEN_TTL,
        };
        let view = auth_session(&record)?;
        purge_expired(&mut data, now);
        enforce_capacity(&mut data, &username);
        data.accesses.insert(
            access_token,
            AccessRecord {
                session_token: token.clone(),
                expires_at: record.access_expires_at,
            },
        );
        data.sessions.insert(token.clone(), record);
        Ok(IssuedSession { token, view })
    }

    pub(crate) fn lookup(&self, token: &str) -> Result<Option<SessionView>, SessionError> {
        let now = Instant::now();
        let mut data = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?;
        purge_expired(&mut data, now);
        let Some(session_token) = data
            .accesses
            .get(token)
            .map(|access| access.session_token.clone())
        else {
            return Ok(None);
        };
        let Some(record) = data.sessions.get_mut(&session_token) else {
            return Ok(None);
        };
        record.last_seen = now;
        session_view(record.username.clone(), record.expires_at_utc).map(Some)
    }

    /// 只由同源刷新 handler 使用 Cookie 凭据。并发刷新复用当前访问 token，不让其他 tab 失效。
    pub(crate) fn refresh(&self, token: &str) -> Result<Option<AuthSession>, SessionError> {
        self.refresh_at(token, Instant::now(), SystemTime::now())
    }

    fn refresh_at(
        &self,
        token: &str,
        now: Instant,
        now_utc: SystemTime,
    ) -> Result<Option<AuthSession>, SessionError> {
        let mut data = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?;
        purge_expired(&mut data, now);
        let Some(record) = data.sessions.get(token) else {
            return Ok(None);
        };
        let renew = record.access_expires_at < record.expires_at
            && record.access_expires_at.saturating_duration_since(now) <= ACCESS_RENEW_WINDOW;
        let replacement = if renew { Some(random_token()?) } else { None };
        if replacement.as_ref().is_some_and(|value| {
            data.accesses.contains_key(value) || data.sessions.contains_key(value)
        }) {
            return Err(SessionError::RandomSource);
        }
        let record = data
            .sessions
            .get_mut(token)
            .ok_or(SessionError::Unavailable)?;
        if let Some(replacement) = replacement {
            let ttl = ACCESS_TOKEN_TTL.min(record.expires_at.saturating_duration_since(now));
            record.access_token = replacement;
            record.access_expires_at = now + ttl;
            record.access_expires_at_utc = now_utc + ttl;
        }
        let view = auth_session(record)?;
        let expires_at = record.access_expires_at;
        record.last_seen = now;
        // 旧访问 token 只活到原期限，保留在途请求；窗口小于 TTL，索引每会话最多两项。
        data.accesses.insert(
            view.access_token.clone(),
            AccessRecord {
                session_token: token.to_owned(),
                expires_at,
            },
        );
        Ok(Some(view))
    }

    pub(crate) fn revoke(&self, token: &str) {
        let mut changed = false;
        if let Ok(mut data) = self.sessions.lock() {
            purge_expired(&mut data, Instant::now());
            if let Some(session_token) = data
                .accesses
                .get(token)
                .map(|access| access.session_token.clone())
            {
                remove_session(&mut data, &session_token);
                changed = true;
            }
        }
        if changed {
            self.notify_change();
        }
    }

    pub(crate) fn revoke_all(&self) {
        let mut changed = false;
        if let Ok(mut data) = self.sessions.lock() {
            changed = !data.sessions.is_empty() || !data.websocket_tickets.is_empty();
            data.sessions.clear();
            data.accesses.clear();
            data.websocket_tickets.clear();
        }
        if changed {
            self.notify_change();
        }
    }

    /// Bearer 只用于签发短期单次 ticket；浏览器握手不读取业务 Cookie 或 URL query。
    pub(super) fn issue_websocket_ticket(
        &self,
        access_token: &str,
    ) -> Result<Option<WebSocketTicket>, SessionError> {
        let now = Instant::now();
        let now_utc = SystemTime::now();
        let mut data = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?;
        purge_expired(&mut data, now);
        let Some(session_token) = data
            .accesses
            .get(access_token)
            .map(|access| access.session_token.clone())
        else {
            return Ok(None);
        };
        let Some(record) = data.sessions.get_mut(&session_token) else {
            return Ok(None);
        };
        record.last_seen = now;

        let session_ticket_count = data
            .websocket_tickets
            .values()
            .filter(|ticket| ticket.session_token == session_token)
            .count();
        if data.websocket_tickets.len() >= WS_TICKET_GLOBAL_CAPACITY
            || session_ticket_count >= WS_TICKET_PER_SESSION_CAPACITY
        {
            return Err(SessionError::Capacity);
        }
        let ticket = random_token()?;
        if data.sessions.contains_key(&ticket)
            || data.accesses.contains_key(&ticket)
            || data.websocket_tickets.contains_key(&ticket)
        {
            return Err(SessionError::RandomSource);
        }
        let expires_at_ms = now_utc
            .checked_add(WS_TICKET_TTL)
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .and_then(|value| u64::try_from(value.as_millis()).ok())
            .ok_or(SessionError::TimeFormat)?;
        data.websocket_tickets.insert(
            ticket.clone(),
            WebSocketTicketRecord {
                session_token,
                expires_at: now + WS_TICKET_TTL,
            },
        );
        Ok(Some(WebSocketTicket {
            ticket,
            expires_at_ms,
        }))
    }

    /// ticket 单次消费，并只返回服务端 session 主键供长连接持续复核。
    pub(super) fn consume_websocket_ticket(
        &self,
        ticket: &str,
    ) -> Result<Option<WebSocketSession>, SessionError> {
        let now = Instant::now();
        let mut data = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?;
        purge_expired(&mut data, now);
        let Some(ticket) = data.websocket_tickets.remove(ticket) else {
            return Ok(None);
        };
        let Some(record) = data.sessions.get_mut(&ticket.session_token) else {
            return Ok(None);
        };
        record.last_seen = now;
        Ok(Some(WebSocketSession(ticket.session_token)))
    }

    /// 心跳和撤销通知都复核主 session；访问 token 轮换不会误杀已建立连接。
    pub(super) fn validate_websocket_session(
        &self,
        session: &WebSocketSession,
    ) -> Result<bool, SessionError> {
        let now = Instant::now();
        let mut data = self
            .sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?;
        purge_expired(&mut data, now);
        let Some(record) = data.sessions.get_mut(&session.0) else {
            return Ok(false);
        };
        record.last_seen = now;
        Ok(true)
    }

    pub(super) fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn notify_change(&self) {
        self.changes
            .send_modify(|value| *value = value.saturating_add(1));
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
        let mut values = Cookie::split_parse(header)
            .filter_map(Result::ok)
            .filter(|cookie| cookie.name() == self.cookie_name());
        let token = values.next()?.value().to_owned();
        if values.next().is_some() || !valid_token(&token) {
            return None;
        }
        Some(token)
    }
}

/// 两类凭据均为 32 字节随机值的 base64url，不接受空白、拼接或无限长 token。
pub(crate) fn valid_token(token: &str) -> bool {
    token.len() == 43
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn random_token() -> Result<String, SessionError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| SessionError::RandomSource)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn remove_session(data: &mut SessionData, token: &str) {
    data.sessions.remove(token);
    data.accesses
        .retain(|_, access| access.session_token != token);
    data.websocket_tickets
        .retain(|_, ticket| ticket.session_token != token);
}

fn purge_expired(data: &mut SessionData, now: Instant) {
    data.sessions.retain(|_, session| {
        now < session.expires_at && now.duration_since(session.last_seen) < SESSION_IDLE_TTL
    });
    data.accesses.retain(|_, access| {
        now < access.expires_at && data.sessions.contains_key(&access.session_token)
    });
    data.websocket_tickets.retain(|_, ticket| {
        now < ticket.expires_at && data.sessions.contains_key(&ticket.session_token)
    });
}

fn enforce_capacity(data: &mut SessionData, username: &str) {
    while data.sessions.len() >= SESSION_GLOBAL_CAPACITY
        || data
            .sessions
            .values()
            .filter(|session| session.username == username)
            .count()
            >= SESSION_PER_USER_CAPACITY
    {
        let candidate = data
            .sessions
            .iter()
            .filter(|(_, session)| {
                data.sessions.len() >= SESSION_GLOBAL_CAPACITY || session.username == username
            })
            .min_by_key(|(_, session)| session.created_at)
            .map(|(token, _)| token.clone());
        let Some(token) = candidate else {
            break;
        };
        remove_session(data, &token);
    }
}

fn auth_session(record: &SessionRecord) -> Result<AuthSession, SessionError> {
    let millis = record
        .access_expires_at_utc
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SessionError::TimeFormat)?
        .as_millis();
    if millis > 9_007_199_254_740_991 {
        return Err(SessionError::TimeFormat);
    }
    Ok(AuthSession {
        session: session_view(record.username.clone(), record.expires_at_utc)?,
        access_token: record.access_token.clone(),
        token_type: "Bearer",
        access_expires_at_ms: millis as u64,
    })
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
    #[error("session capacity is exhausted")]
    Capacity,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(store.lookup(&issued.token).unwrap().is_none());
        assert!(store.refresh(&issued.view.access_token).unwrap().is_none());
        assert!(store.lookup(&issued.view.access_token).unwrap().is_some());
        store.revoke(&issued.view.access_token);
        assert!(store.lookup(&issued.view.access_token).unwrap().is_none());
        assert!(store.refresh(&issued.token).unwrap().is_none());
    }

    #[test]
    fn refreshing_reuses_live_access_then_renews_without_invalidating_inflight_requests() {
        let store = SessionStore::new(false);
        let issued = store.issue("admin".to_owned()).unwrap();
        let first = store.refresh(&issued.token).unwrap().unwrap();
        assert_eq!(first.access_token, issued.view.access_token);
        let now = Instant::now() + ACCESS_TOKEN_TTL - ACCESS_RENEW_WINDOW;
        let next = store
            .refresh_at(
                &issued.token,
                now,
                SystemTime::now() + ACCESS_TOKEN_TTL - ACCESS_RENEW_WINDOW,
            )
            .unwrap()
            .unwrap();
        assert_ne!(first.access_token, next.access_token);
        assert!(store.lookup(&first.access_token).unwrap().is_some());
        assert!(store.lookup(&next.access_token).unwrap().is_some());
        assert_eq!(store.sessions.lock().unwrap().accesses.len(), 2);
        store.revoke(&first.access_token);
        assert!(store.refresh(&issued.token).unwrap().is_none());
        assert!(store.lookup(&next.access_token).unwrap().is_none());
    }

    #[test]
    fn access_expiry_refresh_idle_expiry_and_absolute_expiry_are_independent() {
        let store = SessionStore::new(false);
        let issued = store.issue("admin".to_owned()).unwrap();
        store
            .sessions
            .lock()
            .unwrap()
            .accesses
            .get_mut(&issued.view.access_token)
            .unwrap()
            .expires_at = Instant::now();
        assert!(store.lookup(&issued.view.access_token).unwrap().is_none());
        let now = Instant::now() + ACCESS_TOKEN_TTL;
        assert!(
            store
                .refresh_at(&issued.token, now, SystemTime::now() + ACCESS_TOKEN_TTL)
                .unwrap()
                .is_some()
        );
        {
            let mut data = store.sessions.lock().unwrap();
            data.sessions.get_mut(&issued.token).unwrap().last_seen =
                Instant::now() - SESSION_IDLE_TTL;
        }
        assert!(store.refresh(&issued.token).unwrap().is_none());
        let issued = store.issue("admin".to_owned()).unwrap();
        store
            .sessions
            .lock()
            .unwrap()
            .sessions
            .get_mut(&issued.token)
            .unwrap()
            .expires_at = Instant::now();
        assert!(store.lookup(&issued.view.access_token).unwrap().is_none());
        assert!(store.refresh(&issued.token).unwrap().is_none());
    }

    #[test]
    fn renewals_near_absolute_expiry_and_capacity_eviction_remain_bounded() {
        let store = SessionStore::new(false);
        let issued = store.issue("admin".to_owned()).unwrap();
        {
            let mut data = store.sessions.lock().unwrap();
            let record = data.sessions.get_mut(&issued.token).unwrap();
            record.expires_at = Instant::now() + Duration::from_secs(10);
            record.access_expires_at = record.expires_at;
        }
        for _ in 0..20 {
            store.refresh(&issued.token).unwrap().unwrap();
        }
        assert_eq!(store.sessions.lock().unwrap().accesses.len(), 1);
        for _ in 0..SESSION_PER_USER_CAPACITY {
            store.issue("admin".to_owned()).unwrap();
        }
        assert!(store.lookup(&issued.view.access_token).unwrap().is_none());
        assert!(store.refresh(&issued.token).unwrap().is_none());
        assert_eq!(
            store.sessions.lock().unwrap().accesses.len(),
            SESSION_PER_USER_CAPACITY
        );
        store.revoke_all();
        let data = store.sessions.lock().unwrap();
        assert!(data.sessions.is_empty() && data.accesses.is_empty());
    }

    #[test]
    fn expired_bearer_cannot_revoke_a_refreshable_session() {
        let store = SessionStore::new(false);
        let issued = store.issue("admin".to_owned()).unwrap();
        {
            let mut data = store.sessions.lock().unwrap();
            data.accesses
                .get_mut(&issued.view.access_token)
                .unwrap()
                .expires_at = Instant::now();
            data.sessions
                .get_mut(&issued.token)
                .unwrap()
                .access_expires_at = Instant::now();
        }
        store.revoke(&issued.view.access_token);
        let renewed = store.refresh(&issued.token).unwrap().unwrap();
        assert_ne!(renewed.access_token, issued.view.access_token);
        assert!(store.lookup(&renewed.access_token).unwrap().is_some());
    }

    #[test]
    fn global_eviction_removes_refresh_and_access_indexes_together() {
        let store = SessionStore::new(false);
        let first = store.issue("first".to_owned()).unwrap();
        for index in 0..SESSION_GLOBAL_CAPACITY {
            store.issue(format!("user-{index}")).unwrap();
        }
        assert!(store.lookup(&first.view.access_token).unwrap().is_none());
        assert!(store.refresh(&first.token).unwrap().is_none());
        let data = store.sessions.lock().unwrap();
        assert_eq!(data.sessions.len(), SESSION_GLOBAL_CAPACITY);
        assert_eq!(data.accesses.len(), SESSION_GLOBAL_CAPACITY);
    }

    #[test]
    fn websocket_ticket_is_bearer_derived_single_use_and_revocable() {
        let store = SessionStore::new(false);
        let issued = store.issue("admin".to_owned()).unwrap();
        let ticket = store
            .issue_websocket_ticket(&issued.view.access_token)
            .unwrap()
            .unwrap();
        assert!(
            ticket.expires_at_ms
                > u64::try_from(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis()
                )
                .unwrap()
        );
        let session = store
            .consume_websocket_ticket(&ticket.ticket)
            .unwrap()
            .unwrap();
        assert!(
            store
                .consume_websocket_ticket(&ticket.ticket)
                .unwrap()
                .is_none()
        );
        assert!(store.validate_websocket_session(&session).unwrap());

        store.revoke(&issued.view.access_token);
        assert!(!store.validate_websocket_session(&session).unwrap());
    }

    #[test]
    fn websocket_tickets_are_bounded_per_session_and_expire_before_use() {
        let store = SessionStore::new(false);
        let issued = store.issue("admin".to_owned()).unwrap();
        let mut tickets = Vec::new();
        for _ in 0..WS_TICKET_PER_SESSION_CAPACITY {
            tickets.push(
                store
                    .issue_websocket_ticket(&issued.view.access_token)
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(matches!(
            store.issue_websocket_ticket(&issued.view.access_token),
            Err(SessionError::Capacity)
        ));
        store
            .sessions
            .lock()
            .unwrap()
            .websocket_tickets
            .get_mut(&tickets[0].ticket)
            .unwrap()
            .expires_at = Instant::now();
        assert!(
            store
                .consume_websocket_ticket(&tickets[0].ticket)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .issue_websocket_ticket(&issued.view.access_token)
                .unwrap()
                .is_some()
        );
    }
}
