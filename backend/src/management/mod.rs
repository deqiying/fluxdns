//! 独立于 DoH 数据面的 WebUI Management control plane。

mod assets;
mod auth;
mod config_api;
mod config_query;
pub(crate) mod contract;
mod errors;
mod events;
mod memory;
mod memory_os;
pub(crate) mod memory_sources;
mod metrics;
mod query;
mod router;
mod server;
mod session;

use std::sync::Arc;

use auth::AuthState;
use events::EventHub;
use session::SessionStore;

use crate::config::resolve::ResolvedWebUiUser;

pub(crate) use metrics::MetricsOwner;
pub(crate) use query::ManagementHistoryDependencies;
pub(crate) use server::{ManagementBuildError, ManagementQueryDependencies, ManagementService};

/// 由 DNS service 同生命周期持有的认证状态与会话。
pub(crate) struct ManagementRuntime {
    auth: Arc<AuthState>,
    sessions: Arc<SessionStore>,
    events: Option<Arc<EventHub>>,
}

impl ManagementRuntime {
    fn new(
        auth: Arc<AuthState>,
        sessions: Arc<SessionStore>,
        events: Option<Arc<EventHub>>,
    ) -> Self {
        Self {
            auth,
            sessions,
            events,
        }
    }

    /// Runtime 换代后同步认证用户；用户内容变化（只会来自显式 reload）时撤销全部会话。
    pub(crate) fn reconcile_users(&self, users: &[ResolvedWebUiUser]) {
        if self.auth.replace(users) {
            self.sessions.revoke_all();
        }
    }

    pub(crate) fn shutdown(&self) {
        self.sessions.revoke_all();
        if let Some(events) = &self.events {
            events.shutdown();
        }
    }
}
