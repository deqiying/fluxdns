//! 浏览器 Management WebSocket 的鉴权、连接限额和实时指标订阅。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, watch};

use super::contract::{
    ClientMessage, CommitCursor, DecimalU64, QueryFilter, REPLAY_BYTES, REPLAY_RECORDS,
    REPLAY_SECONDS, ResyncReason, Revision, ServerMessage, WS_CONNECTION_CAPACITY,
    WS_CONNECTIONS_PER_SESSION, WS_HEARTBEAT_SECONDS, WS_IDLE_SECONDS,
    WS_INBOUND_MESSAGES_PER_MINUTE, WS_PROTOCOL_VERSION, WS_QUEUE_BYTES, WS_QUEUE_MESSAGES,
    WS_SUBSCRIPTIONS_PER_CONNECTION, WS_WRITE_TIMEOUT_SECONDS,
};
use super::metrics::MetricsOwner;
use super::query::ManagementQueryService;
use super::router::{
    AuthServices, RequestId, origin_is_allowed, session_token, v2_error_response,
    validate_v2_mutating_request,
};
use super::session::{SessionStore, WebSocketSession, valid_token};
use crate::config::store::ConfigStore;
use crate::storage::{DetailCommitCursor, DetailCommitNotification, DetailShardStore};

pub(super) const WS_SUBPROTOCOL: &str = "fluxdns.v1";
const WS_TICKET_PROTOCOL_PREFIX: &str = "fluxdns.ticket.";
const WS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(WS_HEARTBEAT_SECONDS);
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(WS_IDLE_SECONDS);
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(WS_WRITE_TIMEOUT_SECONDS);

pub(super) fn protected_routes() -> Router<Arc<AuthServices>> {
    Router::new().route("/api/v2/events/ticket", post(issue_ticket))
}

pub(super) fn upgrade_routes() -> Router<Arc<AuthServices>> {
    Router::new().route("/api/v2/events", get(upgrade))
}

struct ConnectionState {
    total: usize,
    per_session: HashMap<String, usize>,
}

struct QueryEventSource {
    queries: Arc<ManagementQueryService>,
    config_store: Arc<ConfigStore>,
    detail_store: Arc<DetailShardStore>,
}

struct ReplayEntry {
    received_at: Instant,
    notification: Arc<DetailCommitNotification>,
    bytes: usize,
}

struct ReplayState {
    epoch: String,
    current_sequence: u64,
    retention_revision: u64,
    floor_sequence: u64,
    floor_reason: ResyncReason,
    records: usize,
    bytes: usize,
    entries: VecDeque<ReplayEntry>,
}

impl ReplayState {
    fn new(cursor: DetailCommitCursor, retention_revision: u64) -> Self {
        Self {
            epoch: cursor.epoch,
            current_sequence: cursor.sequence,
            retention_revision,
            floor_sequence: cursor.sequence,
            floor_reason: ResyncReason::CursorExpired,
            records: 0,
            bytes: 0,
            entries: VecDeque::new(),
        }
    }

    fn mark_gap(&mut self, cursor: DetailCommitCursor, reason: ResyncReason) {
        self.epoch = cursor.epoch;
        self.current_sequence = cursor.sequence;
        self.floor_sequence = cursor.sequence;
        self.floor_reason = reason;
        self.records = 0;
        self.bytes = 0;
        self.entries.clear();
    }

    fn push(&mut self, notification: Arc<DetailCommitNotification>, now: Instant) {
        if notification.cursor.epoch != self.epoch
            || notification.cursor.sequence != self.current_sequence.saturating_add(1)
        {
            self.mark_gap(notification.cursor.clone(), ResyncReason::ObservationGap);
            return;
        }
        let bytes = notification
            .records
            .iter()
            .map(|record| record.estimated_replay_bytes())
            .sum::<usize>();
        self.current_sequence = notification.cursor.sequence;
        self.records = self.records.saturating_add(notification.records.len());
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.push_back(ReplayEntry {
            received_at: now,
            notification,
            bytes,
        });
        self.prune(now);
        while self.records > REPLAY_RECORDS || self.bytes > REPLAY_BYTES {
            self.pop_front(ResyncReason::BufferOverflow);
        }
    }

    fn prune(&mut self, now: Instant) {
        while self.entries.front().is_some_and(|entry| {
            now.duration_since(entry.received_at) >= Duration::from_secs(REPLAY_SECONDS)
        }) {
            self.pop_front(ResyncReason::CursorExpired);
        }
    }

    fn pop_front(&mut self, reason: ResyncReason) {
        let Some(entry) = self.entries.pop_front() else {
            return;
        };
        self.records = self
            .records
            .saturating_sub(entry.notification.records.len());
        self.bytes = self.bytes.saturating_sub(entry.bytes);
        self.floor_sequence = self.floor_sequence.max(entry.notification.cursor.sequence);
        self.floor_reason = reason;
    }
}

enum ReplayLookup {
    Ready(Vec<Arc<DetailCommitNotification>>),
    Pending,
    Resync(ResyncReason),
}

struct QuerySubscription {
    subscription_id: Revision,
    filter: QueryFilter,
    after: CommitCursor,
    retention_revision: Revision,
}

enum QueryUpdate {
    Active,
    Pending,
    Resync(ResyncReason),
    Closed,
}

/// 进程级连接 owner；所有页面共享指标 owner，连接不会各自启动 OS 采样任务。
pub(super) struct EventHub {
    sessions: Arc<SessionStore>,
    metrics: Arc<MetricsOwner>,
    query_source: Option<QueryEventSource>,
    replay: Mutex<ReplayState>,
    commits: Mutex<Option<broadcast::Receiver<Arc<DetailCommitNotification>>>>,
    connections: Arc<Mutex<ConnectionState>>,
    shutdown: watch::Sender<bool>,
    next_connection_id: AtomicU64,
}

impl EventHub {
    pub(super) fn new(
        sessions: Arc<SessionStore>,
        metrics: Arc<MetricsOwner>,
        queries: Arc<ManagementQueryService>,
        config_store: Arc<ConfigStore>,
    ) -> Result<Self, &'static str> {
        let detail_store = queries.detail_store();
        let cursor = detail_store.detail_commit_cursor();
        let retention_revision = detail_store.detail_retention_revision();
        let commits = detail_store.subscribe_commits();
        Self::build(
            sessions,
            metrics,
            cursor,
            retention_revision,
            Some(QueryEventSource {
                queries,
                config_store,
                detail_store,
            }),
            Some(commits),
        )
    }

    fn build(
        sessions: Arc<SessionStore>,
        metrics: Arc<MetricsOwner>,
        cursor: DetailCommitCursor,
        retention_revision: u64,
        query_source: Option<QueryEventSource>,
        commits: Option<broadcast::Receiver<Arc<DetailCommitNotification>>>,
    ) -> Result<Self, &'static str> {
        Revision::try_from(cursor.epoch.clone())?;
        let (shutdown, _) = watch::channel(false);
        Ok(Self {
            sessions,
            metrics,
            query_source,
            replay: Mutex::new(ReplayState::new(cursor, retention_revision)),
            commits: Mutex::new(commits),
            connections: Arc::new(Mutex::new(ConnectionState {
                total: 0,
                per_session: HashMap::new(),
            })),
            shutdown,
            next_connection_id: AtomicU64::new(1),
        })
    }

    #[cfg(test)]
    fn without_queries(
        sessions: Arc<SessionStore>,
        metrics: Arc<MetricsOwner>,
        epoch: String,
    ) -> Result<Self, &'static str> {
        Self::build(
            sessions,
            metrics,
            DetailCommitCursor { epoch, sequence: 0 },
            0,
            None,
            None,
        )
    }

    /// Management supervisor 启动时唯一接管 commit receiver，关闭时由同一 owner 回收。
    pub(super) fn start_collector(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let receiver = self.commits.lock().ok()?.take()?;
        let events = Arc::clone(self);
        Some(tokio::spawn(async move {
            events.collect_commits(receiver).await
        }))
    }

    async fn collect_commits(
        self: Arc<Self>,
        mut commits: broadcast::Receiver<Arc<DetailCommitNotification>>,
    ) {
        let mut shutdown = self.shutdown.subscribe();
        let mut retention_tick = tokio::time::interval(Duration::from_secs(1));
        retention_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                commit = commits.recv() => match commit {
                    Ok(commit) => {
                        if let Ok(mut replay) = self.replay.lock() {
                            replay.push(commit, Instant::now());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => self.mark_replay_gap(),
                    Err(broadcast::error::RecvError::Closed) => {
                        self.mark_replay_gap();
                        break;
                    }
                },
                _ = retention_tick.tick() => {
                    if let Some(source) = &self.query_source
                        && let Ok(mut replay) = self.replay.lock()
                    {
                        replay.retention_revision = source.detail_store.detail_retention_revision();
                        replay.prune(Instant::now());
                    }
                }
            }
        }
    }

    fn mark_replay_gap(&self) {
        let Some(source) = &self.query_source else {
            return;
        };
        let cursor = source.detail_store.detail_commit_cursor();
        if let Ok(mut replay) = self.replay.lock() {
            replay.mark_gap(cursor, ResyncReason::ObservationGap);
        }
    }

    fn current_epoch(&self) -> Option<Revision> {
        self.replay
            .lock()
            .ok()
            .and_then(|replay| Revision::try_from(replay.epoch.clone()).ok())
    }

    fn replay_after(&self, after: &CommitCursor, retention_revision: &Revision) -> ReplayLookup {
        let Some(source) = &self.query_source else {
            return ReplayLookup::Resync(ResyncReason::ObservationGap);
        };
        let current_store_cursor = source.detail_store.detail_commit_cursor();
        let current_retention = source.detail_store.detail_retention_revision();
        let Ok(expected_retention) = retention_revision.as_str().parse::<u64>() else {
            return ReplayLookup::Resync(ResyncReason::CursorExpired);
        };
        let mut replay = match self.replay.lock() {
            Ok(replay) => replay,
            Err(_) => return ReplayLookup::Resync(ResyncReason::ObservationGap),
        };
        replay.retention_revision = current_retention;
        replay.prune(Instant::now());
        if expected_retention != replay.retention_revision {
            return ReplayLookup::Resync(ResyncReason::RetentionChanged);
        }
        if after.epoch.as_str() != replay.epoch {
            return ReplayLookup::Resync(ResyncReason::EpochChanged);
        }
        let sequence = after.sequence.as_u64();
        if sequence > replay.current_sequence {
            if current_store_cursor.epoch == replay.epoch
                && sequence <= current_store_cursor.sequence
            {
                return ReplayLookup::Pending;
            }
            return ReplayLookup::Resync(ResyncReason::CursorExpired);
        }
        if sequence < replay.floor_sequence {
            return ReplayLookup::Resync(replay.floor_reason.clone());
        }
        ReplayLookup::Ready(
            replay
                .entries
                .iter()
                .filter(|entry| entry.notification.cursor.sequence > sequence)
                .map(|entry| Arc::clone(&entry.notification))
                .collect(),
        )
    }

    fn queue_query_updates(
        &self,
        subscription: &mut QuerySubscription,
        outbound: &Outbound,
    ) -> QueryUpdate {
        let notifications =
            match self.replay_after(&subscription.after, &subscription.retention_revision) {
                ReplayLookup::Ready(notifications) => notifications,
                ReplayLookup::Pending => return QueryUpdate::Pending,
                ReplayLookup::Resync(reason) => return QueryUpdate::Resync(reason),
            };
        let Some(source) = &self.query_source else {
            return QueryUpdate::Resync(ResyncReason::ObservationGap);
        };
        for notification in notifications {
            let (directory_revision, items) = match source.queries.project_committed_records(
                &source.config_store,
                subscription.filter.clone(),
                &notification.records,
            ) {
                Ok(projected) => projected,
                Err(_) => return QueryUpdate::Resync(ResyncReason::ObservationGap),
            };
            let cursor = match commit_cursor(&notification.cursor) {
                Ok(cursor) => cursor,
                Err(reason) => return QueryUpdate::Resync(reason),
            };
            if items.is_empty() {
                if !outbound.json(&ServerMessage::Queries {
                    subscription_id: subscription.subscription_id.clone(),
                    cursor: cursor.clone(),
                    directory_revision,
                    items,
                }) {
                    return QueryUpdate::Closed;
                }
            } else {
                for chunk in items.chunks(100) {
                    if !outbound.json(&ServerMessage::Queries {
                        subscription_id: subscription.subscription_id.clone(),
                        cursor: cursor.clone(),
                        directory_revision: directory_revision.clone(),
                        items: chunk.to_vec(),
                    }) {
                        return QueryUpdate::Closed;
                    }
                }
            }
            subscription.after = cursor;
        }
        QueryUpdate::Active
    }

    pub(super) fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    fn try_acquire(&self, session: &WebSocketSession) -> Option<ConnectionPermit> {
        let mut connections = self.connections.lock().ok()?;
        let session_count = connections
            .per_session
            .get(session.key())
            .copied()
            .unwrap_or(0);
        if connections.total >= WS_CONNECTION_CAPACITY
            || session_count >= WS_CONNECTIONS_PER_SESSION
        {
            return None;
        }
        connections.total += 1;
        *connections
            .per_session
            .entry(session.key().to_owned())
            .or_default() += 1;
        Some(ConnectionPermit {
            connections: Arc::clone(&self.connections),
            session_key: session.key().to_owned(),
        })
    }

    async fn serve(
        self: Arc<Self>,
        socket: WebSocket,
        session: WebSocketSession,
        permit: ConnectionPermit,
    ) {
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let (sink, stream) = socket.split();
        let (outbound, writer) = Outbound::new(sink);
        let connection = self.run_connection(stream, outbound.clone(), session, connection_id);
        connection.await;
        outbound.close(1000, "connection closed");
        let _ = writer.await;
        drop(permit);
    }

    async fn run_connection(
        &self,
        mut stream: SplitStream<WebSocket>,
        outbound: Outbound,
        session: WebSocketSession,
        connection_id: u64,
    ) {
        let Some(epoch) = self.current_epoch() else {
            return;
        };
        if !outbound.json(&ServerMessage::Ready {
            protocol_version: WS_PROTOCOL_VERSION,
            epoch,
        }) {
            return;
        }

        let mut metrics_subscriptions = HashSet::new();
        let mut query_subscriptions = HashMap::new();
        let mut metrics_tick = tokio::time::interval(Duration::from_secs(1));
        metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        metrics_tick.tick().await;
        let mut query_tick = tokio::time::interval(Duration::from_secs(1));
        query_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        query_tick.tick().await;
        let mut heartbeat = tokio::time::interval(WS_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await;
        let mut session_changes = self.sessions.subscribe_changes();
        let mut shutdown = self.shutdown.subscribe();
        let mut pending_nonce = None;
        let mut last_pong = Instant::now();
        let mut heartbeat_sequence = 0_u64;
        let mut inbound = VecDeque::new();

        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        outbound.close(1001, "service shutdown");
                        break;
                    }
                }
                changed = session_changes.changed() => {
                    if changed.is_err() || !self.session_is_valid(&session) {
                        outbound.close(4401, "session expired");
                        break;
                    }
                }
                message = stream.next() => {
                    let Some(message) = message else { break; };
                    let Ok(message) = message else { break; };
                    if !allow_inbound(&mut inbound, Instant::now()) {
                        outbound.close(4429, "message rate exceeded");
                        break;
                    }
                    match message {
                        Message::Text(text) => {
                            let Ok(message) = super::contract::decode_client_message(text.as_bytes()) else {
                                outbound.close(1008, "invalid client message");
                                break;
                            };
                            match message {
                                ClientMessage::SubscribeMetrics { subscription_id } => {
                                    let id = subscription_id.as_str();
                                    if query_subscriptions.contains_key(id)
                                        || (!metrics_subscriptions.contains(id)
                                            && metrics_subscriptions.len() + query_subscriptions.len()
                                                >= WS_SUBSCRIPTIONS_PER_CONNECTION)
                                    {
                                        outbound.close(1008, "subscription limit exceeded");
                                        break;
                                    }
                                    metrics_subscriptions.insert(id.to_owned());
                                    if !outbound.json(&ServerMessage::Metrics {
                                        subscription_id,
                                        data: self.metrics.service_metrics(),
                                    }) {
                                        break;
                                    }
                                }
                                ClientMessage::Unsubscribe { subscription_id } => {
                                    metrics_subscriptions.remove(subscription_id.as_str());
                                    query_subscriptions.remove(subscription_id.as_str());
                                }
                                ClientMessage::Pong { nonce } => {
                                    if pending_nonce.as_deref() != Some(nonce.as_str()) {
                                        outbound.close(1008, "invalid heartbeat nonce");
                                        break;
                                    }
                                    pending_nonce = None;
                                    last_pong = Instant::now();
                                }
                                ClientMessage::SubscribeQueries {
                                    subscription_id,
                                    filter,
                                    after,
                                    retention_revision,
                                } => {
                                    let id = subscription_id.as_str().to_owned();
                                    if metrics_subscriptions.contains(&id)
                                        || (!query_subscriptions.contains_key(&id)
                                            && metrics_subscriptions.len() + query_subscriptions.len()
                                                >= WS_SUBSCRIPTIONS_PER_CONNECTION)
                                    {
                                        outbound.close(1008, "subscription limit exceeded");
                                        break;
                                    }
                                    let mut subscription = QuerySubscription {
                                        subscription_id,
                                        filter,
                                        after,
                                        retention_revision,
                                    };
                                    match self.queue_query_updates(&mut subscription, &outbound) {
                                        QueryUpdate::Active | QueryUpdate::Pending => {
                                            query_subscriptions.insert(id, subscription);
                                        }
                                        QueryUpdate::Resync(reason) => {
                                            if !outbound.json(&ServerMessage::ResyncRequired {
                                                subscription_id: subscription.subscription_id,
                                                reason,
                                            }) {
                                                break;
                                            }
                                        }
                                        QueryUpdate::Closed => break,
                                    }
                                }
                            }
                        }
                        Message::Ping(payload) => {
                            if !outbound.message(Message::Pong(payload)) { break; }
                        }
                        Message::Pong(_) => last_pong = Instant::now(),
                        Message::Close(_) => break,
                        Message::Binary(_) => {
                            outbound.close(1003, "text messages required");
                            break;
                        }
                    }
                }
                _ = metrics_tick.tick() => {
                    for subscription_id in &metrics_subscriptions {
                        let Ok(subscription_id) = Revision::try_from(subscription_id.clone()) else {
                            outbound.close(1011, "subscription state failed");
                            return;
                        };
                        if !outbound.json(&ServerMessage::Metrics {
                            subscription_id,
                            data: self.metrics.service_metrics(),
                        }) {
                            return;
                        }
                    }
                }
                _ = query_tick.tick() => {
                    let ids = query_subscriptions.keys().cloned().collect::<Vec<_>>();
                    for id in ids {
                        let update = {
                            let Some(subscription) = query_subscriptions.get_mut(&id) else { continue; };
                            self.queue_query_updates(subscription, &outbound)
                        };
                        match update {
                            QueryUpdate::Active | QueryUpdate::Pending => {}
                            QueryUpdate::Closed => return,
                            QueryUpdate::Resync(reason) => {
                                let Some(subscription) = query_subscriptions.remove(&id) else { continue; };
                                if !outbound.json(&ServerMessage::ResyncRequired {
                                    subscription_id: subscription.subscription_id,
                                    reason,
                                }) {
                                    return;
                                }
                            }
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    let session_valid = self.session_is_valid(&session);
                    if last_pong.elapsed() >= WS_IDLE_TIMEOUT || !session_valid {
                        outbound.close(if session_valid { 4408 } else { 4401 }, "connection expired");
                        break;
                    }
                    heartbeat_sequence = heartbeat_sequence.saturating_add(1);
                    let nonce = format!("ping:{connection_id}:{heartbeat_sequence}");
                    let Ok(nonce_revision) = Revision::try_from(nonce.clone()) else {
                        outbound.close(1011, "heartbeat state failed");
                        break;
                    };
                    pending_nonce = Some(nonce);
                    if !outbound.json(&ServerMessage::Ping { nonce: nonce_revision }) {
                        break;
                    }
                }
            }
        }
    }

    fn session_is_valid(&self, session: &WebSocketSession) -> bool {
        self.sessions
            .validate_websocket_session(session)
            .unwrap_or(false)
    }
}

struct ConnectionPermit {
    connections: Arc<Mutex<ConnectionState>>,
    session_key: String,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let Ok(mut connections) = self.connections.lock() else {
            return;
        };
        connections.total = connections.total.saturating_sub(1);
        if let Some(value) = connections.per_session.get_mut(&self.session_key) {
            *value = value.saturating_sub(1);
            if *value == 0 {
                connections.per_session.remove(&self.session_key);
            }
        }
    }
}

#[derive(Clone)]
struct Outbound {
    messages: mpsc::Sender<QueuedMessage>,
    bytes: Arc<Semaphore>,
    close: watch::Sender<Option<(u16, &'static str)>>,
}

struct QueuedMessage {
    message: Message,
    _permit: OwnedSemaphorePermit,
}

impl Outbound {
    fn new(sink: SplitSink<WebSocket, Message>) -> (Self, tokio::task::JoinHandle<()>) {
        let (messages, receiver) = mpsc::channel(WS_QUEUE_MESSAGES);
        let (close, close_receiver) = watch::channel(None);
        let outbound = Self {
            messages,
            bytes: Arc::new(Semaphore::new(WS_QUEUE_BYTES)),
            close,
        };
        let writer = tokio::spawn(write_messages(sink, receiver, close_receiver));
        (outbound, writer)
    }

    fn json(&self, message: &ServerMessage) -> bool {
        let Ok(text) = serde_json::to_string(message) else {
            self.close(1011, "message serialization failed");
            return false;
        };
        self.message(Message::Text(text.into()))
    }

    fn message(&self, message: Message) -> bool {
        let byte_len = match &message {
            Message::Text(value) => value.len(),
            Message::Binary(value) | Message::Ping(value) | Message::Pong(value) => value.len(),
            Message::Close(_) => 0,
        };
        let Ok(byte_len) = u32::try_from(byte_len) else {
            self.close(1013, "slow consumer");
            return false;
        };
        let Ok(permit) = Arc::clone(&self.bytes).try_acquire_many_owned(byte_len) else {
            self.close(1013, "slow consumer");
            return false;
        };
        if self
            .messages
            .try_send(QueuedMessage {
                message,
                _permit: permit,
            })
            .is_err()
        {
            self.close(1013, "slow consumer");
            return false;
        }
        true
    }

    fn close(&self, code: u16, reason: &'static str) {
        self.close.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some((code, reason));
            true
        });
    }
}

async fn write_messages(
    mut sink: SplitSink<WebSocket, Message>,
    mut messages: mpsc::Receiver<QueuedMessage>,
    mut close: watch::Receiver<Option<(u16, &'static str)>>,
) {
    loop {
        tokio::select! {
            biased;
            changed = close.changed() => {
                if changed.is_err() { break; }
                let requested_close = *close.borrow();
                if let Some((code, reason)) = requested_close {
                    let _ = tokio::time::timeout(
                        WS_WRITE_TIMEOUT,
                        sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))),
                    ).await;
                    break;
                }
            }
            message = messages.recv() => {
                let Some(message) = message else { break; };
                if !matches!(
                    tokio::time::timeout(WS_WRITE_TIMEOUT, sink.send(message.message)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
        }
    }
}

async fn issue_ticket(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) =
        validate_v2_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Some(access_token) = session_token(&headers) else {
        return v2_error_response(super::contract::ErrorCode::AuthRequired, &request_id);
    };
    match services.sessions.issue_websocket_ticket(&access_token) {
        Ok(Some(ticket)) => (StatusCode::CREATED, Json(ticket)).into_response(),
        Ok(None) => v2_error_response(super::contract::ErrorCode::AuthRequired, &request_id),
        Err(super::session::SessionError::Capacity) => {
            v2_error_response(super::contract::ErrorCode::RateLimited, &request_id)
        }
        Err(_) => v2_error_response(super::contract::ErrorCode::ServiceUnavailable, &request_id),
    }
}

async fn upgrade(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    uri: Uri,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    let Some(events) = &services.events else {
        return v2_error_response(super::contract::ErrorCode::ServiceUnavailable, &request_id);
    };
    if uri.query().is_some()
        || headers.contains_key(AUTHORIZATION)
        || !origin_is_allowed(&headers, &services.public_origin)
    {
        return v2_error_response(super::contract::ErrorCode::Forbidden, &request_id);
    }
    let Some(ticket) = ticket_from_protocols(&websocket) else {
        return v2_error_response(super::contract::ErrorCode::AuthRequired, &request_id);
    };
    let session = match services.sessions.consume_websocket_ticket(&ticket) {
        Ok(Some(session)) => session,
        Ok(None) => {
            return v2_error_response(super::contract::ErrorCode::AuthRequired, &request_id);
        }
        Err(_) => {
            return v2_error_response(super::contract::ErrorCode::ServiceUnavailable, &request_id);
        }
    };
    let Some(permit) = events.try_acquire(&session) else {
        return v2_error_response(super::contract::ErrorCode::RateLimited, &request_id);
    };
    let events = Arc::clone(events);
    websocket
        .protocols([WS_SUBPROTOCOL])
        .write_buffer_size(64 * 1024)
        .max_write_buffer_size(WS_QUEUE_BYTES)
        .max_message_size(super::contract::MAX_WS_FRAME_BYTES)
        .max_frame_size(super::contract::MAX_WS_FRAME_BYTES)
        .on_upgrade(move |socket| events.serve(socket, session, permit))
}

fn ticket_from_protocols(websocket: &WebSocketUpgrade) -> Option<String> {
    let mut protocol = false;
    let mut ticket = None;
    for value in websocket.requested_protocols() {
        let value = value.to_str().ok()?;
        if value == WS_SUBPROTOCOL {
            if protocol {
                return None;
            }
            protocol = true;
        } else if let Some(value) = value.strip_prefix(WS_TICKET_PROTOCOL_PREFIX) {
            if ticket.is_some() || !valid_token(value) {
                return None;
            }
            ticket = Some(value.to_owned());
        } else {
            return None;
        }
    }
    protocol.then_some(ticket).flatten()
}

fn commit_cursor(cursor: &DetailCommitCursor) -> Result<CommitCursor, ResyncReason> {
    Ok(CommitCursor {
        epoch: Revision::try_from(cursor.epoch.clone())
            .map_err(|_| ResyncReason::ObservationGap)?,
        sequence: DecimalU64::from(cursor.sequence),
    })
}

fn allow_inbound(messages: &mut VecDeque<Instant>, now: Instant) -> bool {
    while messages
        .front()
        .is_some_and(|seen| now.duration_since(*seen) >= Duration::from_secs(60))
    {
        messages.pop_front();
    }
    if messages.len() >= WS_INBOUND_MESSAGES_PER_MINUTE {
        return false;
    }
    messages.push_back(now);
    true
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientFrame;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::*;
    use crate::config::migrate::deterministic_hash;
    use crate::config::store::ConfigStore;
    use crate::management::auth::AuthState;
    use crate::management::contract::{ClientMessage, ServerMessage, WebSocketTicket};
    use crate::management::router::{AuthServices, build_router};

    struct TestServer {
        address: SocketAddr,
        origin: String,
        sessions: Arc<SessionStore>,
        events: Arc<EventHub>,
        queries: Option<Arc<ManagementQueryService>>,
        collector: Option<tokio::task::JoinHandle<()>>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
        root: PathBuf,
    }

    impl TestServer {
        async fn start() -> Self {
            let root = PathBuf::from(crate::config::test_support::absolute_path(
                "management-events-real",
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let source = root.join("config.yaml");
            std::fs::write(&source, "version: 2\n").unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let origin = format!("http://{address}");
            let sessions = Arc::new(SessionStore::new(false));
            let events = Arc::new(
                EventHub::without_queries(
                    Arc::clone(&sessions),
                    Arc::new(MetricsOwner::new()),
                    "dqs_test-epoch".to_owned(),
                )
                .unwrap(),
            );
            let services = Arc::new(
                AuthServices::new(
                    Arc::new(AuthState::new(&[]).unwrap()),
                    Arc::clone(&sessions),
                    Arc::new(ConfigStore::new(
                        source.clone(),
                        source,
                        deterministic_hash(b"version: 2\n"),
                    )),
                    origin.clone(),
                    None,
                )
                .with_events(Arc::clone(&events)),
            );
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                axum::serve(
                    listener,
                    build_router(services).into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
            });
            Self {
                address,
                origin,
                sessions,
                events,
                queries: None,
                collector: None,
                stop: Some(stop),
                task,
                root,
            }
        }

        async fn start_with_queries() -> Self {
            let (query_services, root) = super::super::query::tests::test_services().await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let origin = format!("http://{address}");
            let sessions = Arc::new(SessionStore::new(false));
            let queries = Arc::clone(query_services.queries.as_ref().unwrap());
            let events = Arc::new(
                EventHub::new(
                    Arc::clone(&sessions),
                    Arc::new(MetricsOwner::new()),
                    Arc::clone(&queries),
                    Arc::clone(&query_services.config_store),
                )
                .unwrap(),
            );
            let collector = events.start_collector();
            let services = Arc::new(
                AuthServices::new(
                    Arc::clone(&query_services.auth),
                    Arc::clone(&sessions),
                    Arc::clone(&query_services.config_store),
                    origin.clone(),
                    Some(Arc::clone(&queries)),
                )
                .with_events(Arc::clone(&events)),
            );
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                axum::serve(
                    listener,
                    build_router(services).into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
            });
            Self {
                address,
                origin,
                sessions,
                events,
                queries: Some(queries),
                collector,
                stop: Some(stop),
                task,
                root,
            }
        }

        async fn stop(mut self) {
            self.events.shutdown();
            let _ = self.stop.take().unwrap().send(());
            self.task.await.unwrap();
            if let Some(collector) = self.collector.take() {
                collector.await.unwrap();
            }
            if let Some(queries) = &self.queries {
                queries
                    .detail_store()
                    .shutdown(crate::dns::Deadline::new(
                        Instant::now() + Duration::from_secs(5),
                    ))
                    .await
                    .unwrap();
            }
            let _ = std::fs::remove_dir_all(self.root);
        }

        async fn issue_ticket(&self, access_token: &str) -> WebSocketTicket {
            let mut stream = tokio::net::TcpStream::connect(self.address).await.unwrap();
            stream
                .write_all(
                    format!(
                        "POST /api/v2/events/ticket HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        self.address, self.origin, access_token
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 201 Created\r\n"));
            let body = response
                .windows(4)
                .position(|value| value == b"\r\n\r\n")
                .map(|index| &response[index + 4..])
                .unwrap();
            serde_json::from_slice(body).unwrap()
        }

        fn request(
            &self,
            path: &str,
            origin: &str,
            ticket: Option<&str>,
        ) -> axum::http::Request<()> {
            let mut request = format!("ws://{}{path}", self.address)
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
            if let Some(ticket) = ticket {
                request.headers_mut().insert(
                    "sec-websocket-protocol",
                    format!("{WS_SUBPROTOCOL}, {WS_TICKET_PROTOCOL_PREFIX}{ticket}")
                        .parse()
                        .unwrap(),
                );
            }
            request
        }
    }

    #[tokio::test]
    async fn real_http_ticket_and_websocket_metrics_enforce_origin_and_revocation() {
        let server = TestServer::start().await;
        let issued = server.sessions.issue("admin".to_owned()).unwrap();
        let ticket = server.issue_ticket(&issued.view.access_token).await;

        let foreign = server.request(
            "/api/v2/events",
            "http://foreign.example.test",
            Some(&ticket.ticket),
        );
        let error = connect_async(foreign).await.unwrap_err();
        assert_eq!(error_http_status(error), Some(StatusCode::FORBIDDEN));

        let query = server.request(
            &format!("/api/v2/events?token={}", issued.view.access_token),
            &server.origin,
            Some(&ticket.ticket),
        );
        let error = connect_async(query).await.unwrap_err();
        assert_eq!(error_http_status(error), Some(StatusCode::FORBIDDEN));

        let mut cookie_only = server.request("/api/v2/events", &server.origin, None);
        cookie_only.headers_mut().insert(
            "cookie",
            format!("fluxdns_session={}", issued.token).parse().unwrap(),
        );
        let error = connect_async(cookie_only).await.unwrap_err();
        assert_eq!(error_http_status(error), Some(StatusCode::UNAUTHORIZED));

        let mut request = server.request("/api/v2/events", &server.origin, Some(&ticket.ticket));
        request.headers_mut().insert(
            "cookie",
            format!("fluxdns_session={}", issued.token).parse().unwrap(),
        );
        let (mut websocket, response) = connect_async(request).await.unwrap();
        assert_eq!(
            response.headers().get("sec-websocket-protocol").unwrap(),
            WS_SUBPROTOCOL
        );
        let ready = receive_server_message(&mut websocket).await;
        assert!(matches!(
            ready,
            ServerMessage::Ready {
                protocol_version: 1,
                ..
            }
        ));
        websocket
            .send(ClientFrame::Text(
                serde_json::to_string(&ClientMessage::SubscribeMetrics {
                    subscription_id: Revision::try_from("metrics-1".to_owned()).unwrap(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let metrics = receive_server_message(&mut websocket).await;
        assert!(matches!(metrics, ServerMessage::Metrics { .. }));

        server.sessions.revoke(&issued.view.access_token);
        let close = tokio::time::timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ClientFrame::Close(Some(close)) = close else {
            panic!("expected authentication close frame");
        };
        assert_eq!(u16::from(close.code), 4401);

        let reused = server.request("/api/v2/events", &server.origin, Some(&ticket.ticket));
        let error = connect_async(reused).await.unwrap_err();
        assert_eq!(error_http_status(error), Some(StatusCode::UNAUTHORIZED));
        server.stop().await;
    }

    #[tokio::test]
    async fn real_websocket_queries_replay_disconnects_and_resyncs_after_retention_change() {
        let server = TestServer::start_with_queries().await;
        let issued = server.sessions.issue("admin".to_owned()).unwrap();
        let queries = server.queries.as_ref().unwrap();
        let detail_store = queries.detail_store();
        let initial = detail_store.detail_commit_cursor();
        let retention_revision = detail_store.detail_retention_revision();

        let ticket = server.issue_ticket(&issued.view.access_token).await;
        let request = server.request("/api/v2/events", &server.origin, Some(&ticket.ticket));
        let (mut websocket, _) = connect_async(request).await.unwrap();
        assert!(matches!(
            receive_server_message(&mut websocket).await,
            ServerMessage::Ready { .. }
        ));
        websocket
            .send(ClientFrame::Text(
                serde_json::to_string(&subscribe_queries(
                    "queries-1",
                    &initial,
                    retention_revision,
                ))
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        detail_store
            .write_records(
                i32::try_from(super::super::query::tests::HISTORY_DAY + 2).unwrap(),
                &[super::super::query::tests::history_record(
                    super::super::query::tests::HISTORY_DAY + 2,
                    1,
                    crate::ports::storage::StatsSource::Upstream,
                )],
                crate::dns::Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        let first = receive_server_message(&mut websocket).await;
        let first_cursor = match first {
            ServerMessage::Queries { cursor, items, .. } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].qname, "direct.example.");
                cursor
            }
            message => panic!("expected query update, got {message:?}"),
        };
        websocket.close(None).await.unwrap();

        detail_store
            .write_records(
                i32::try_from(super::super::query::tests::HISTORY_DAY + 2).unwrap(),
                &[super::super::query::tests::history_record(
                    super::super::query::tests::HISTORY_DAY + 2,
                    2,
                    crate::ports::storage::StatsSource::Upstream,
                )],
                crate::dns::Deadline::new(Instant::now() + Duration::from_secs(5)),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ticket = server.issue_ticket(&issued.view.access_token).await;
        let request = server.request("/api/v2/events", &server.origin, Some(&ticket.ticket));
        let (mut websocket, _) = connect_async(request).await.unwrap();
        assert!(matches!(
            receive_server_message(&mut websocket).await,
            ServerMessage::Ready { .. }
        ));
        websocket
            .send(ClientFrame::Text(
                serde_json::to_string(&ClientMessage::SubscribeQueries {
                    subscription_id: Revision::try_from("queries-2".to_owned()).unwrap(),
                    filter: query_filter(),
                    after: first_cursor.clone(),
                    retention_revision: Revision::try_from(retention_revision.to_string()).unwrap(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let replay = receive_server_message(&mut websocket).await;
        match replay {
            ServerMessage::Queries { cursor, items, .. } => {
                assert_eq!(items.len(), 1);
                assert!(cursor.sequence.as_u64() > first_cursor.sequence.as_u64());
            }
            message => panic!("expected replayed query update, got {message:?}"),
        }

        detail_store.publish_retired_before(
            i32::try_from(super::super::query::tests::HISTORY_DAY + 1).unwrap(),
        );
        websocket
            .send(ClientFrame::Text(
                serde_json::to_string(&ClientMessage::SubscribeQueries {
                    subscription_id: Revision::try_from("queries-retention".to_owned()).unwrap(),
                    filter: query_filter(),
                    after: first_cursor,
                    retention_revision: Revision::try_from(retention_revision.to_string()).unwrap(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            receive_server_message(&mut websocket).await,
            ServerMessage::ResyncRequired {
                reason: ResyncReason::RetentionChanged,
                ..
            }
        ));
        server.stop().await;
    }

    #[test]
    fn connection_rate_and_slow_consumer_limits_are_bounded() {
        let sessions = Arc::new(SessionStore::new(false));
        let issued = sessions.issue("admin".to_owned()).unwrap();
        let session = sessions
            .consume_websocket_ticket(
                &sessions
                    .issue_websocket_ticket(&issued.view.access_token)
                    .unwrap()
                    .unwrap()
                    .ticket,
            )
            .unwrap()
            .unwrap();
        let hub = EventHub::without_queries(
            sessions,
            Arc::new(MetricsOwner::new()),
            "dqs_limit-test".to_owned(),
        )
        .unwrap();
        let permits = (0..WS_CONNECTIONS_PER_SESSION)
            .map(|_| hub.try_acquire(&session).unwrap())
            .collect::<Vec<_>>();
        assert!(hub.try_acquire(&session).is_none());
        drop(permits);
        assert!(hub.try_acquire(&session).is_some());

        let mut inbound = VecDeque::new();
        let now = Instant::now();
        for _ in 0..WS_INBOUND_MESSAGES_PER_MINUTE {
            assert!(allow_inbound(&mut inbound, now));
        }
        assert!(!allow_inbound(&mut inbound, now));

        let (messages, _receiver) = mpsc::channel(1);
        let (close, close_receiver) = watch::channel(None);
        let outbound = Outbound {
            messages,
            bytes: Arc::new(Semaphore::new(WS_QUEUE_BYTES)),
            close,
        };
        assert!(outbound.message(Message::Text("first".into())));
        assert!(!outbound.message(Message::Text("second".into())));
        assert_eq!(*close_receiver.borrow(), Some((1013, "slow consumer")));
    }

    #[test]
    fn replay_watermark_expires_old_entries_and_marks_sequence_gaps() {
        let mut replay = ReplayState::new(
            DetailCommitCursor {
                epoch: "dqs_replay-test".to_owned(),
                sequence: 0,
            },
            1,
        );
        let now = Instant::now();
        replay.push(
            empty_notification("dqs_replay-test", 1),
            now - Duration::from_secs(REPLAY_SECONDS),
        );
        replay.push(empty_notification("dqs_replay-test", 2), now);
        assert_eq!(replay.floor_sequence, 1);
        assert!(matches!(replay.floor_reason, ResyncReason::CursorExpired));
        assert_eq!(replay.entries.len(), 1);

        replay.push(empty_notification("dqs_replay-test", 4), now);
        assert_eq!(replay.current_sequence, 4);
        assert_eq!(replay.floor_sequence, 4);
        assert!(replay.entries.is_empty());
        assert!(matches!(replay.floor_reason, ResyncReason::ObservationGap));
    }

    fn empty_notification(epoch: &str, sequence: u64) -> Arc<DetailCommitNotification> {
        Arc::new(DetailCommitNotification {
            cursor: DetailCommitCursor {
                epoch: epoch.to_owned(),
                sequence,
            },
            records: Arc::from([]),
        })
    }

    fn subscribe_queries(
        subscription_id: &str,
        after: &DetailCommitCursor,
        retention_revision: u64,
    ) -> ClientMessage {
        ClientMessage::SubscribeQueries {
            subscription_id: Revision::try_from(subscription_id.to_owned()).unwrap(),
            filter: query_filter(),
            after: commit_cursor(after).unwrap(),
            retention_revision: Revision::try_from(retention_revision.to_string()).unwrap(),
        }
    }

    fn query_filter() -> QueryFilter {
        let day = super::super::query::tests::HISTORY_DAY + 2;
        QueryFilter {
            from_ms: day * 86_400_000,
            to_ms: (day + 1) * 86_400_000,
            client_id: None,
            client_ip: None,
            qname: Some("direct.example.".to_owned()),
            transport: None,
            matched_client_id: None,
            client_name: None,
            qtype: None,
            rcode: None,
            source: None,
            outcome: None,
            cache: None,
        }
    }

    async fn receive_server_message(
        websocket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> ServerMessage {
        let message = tokio::time::timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ClientFrame::Text(text) = message else {
            panic!("expected text server message, got {message:?}");
        };
        serde_json::from_str(&text).unwrap()
    }

    fn error_http_status(error: tokio_tungstenite::tungstenite::Error) -> Option<StatusCode> {
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            return None;
        };
        Some(response.status())
    }
}
