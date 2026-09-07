//! 独立于 DoH 数据面的 WebUI Management control plane。

mod assets;
mod auth;
pub(crate) mod contract;
mod query;
mod router;
mod server;
mod session;

use std::sync::Arc;

use auth::AuthState;
use session::SessionStore;

use crate::config::resolve::ResolvedWebUiUser;
use crate::config::store::ConfigStore;

pub(crate) use server::{ManagementBuildError, ManagementQueryDependencies, ManagementService};

/// 由 DNS service 同生命周期持有的认证状态和配置写入协调器。
pub(crate) struct ManagementRuntime {
    auth: Arc<AuthState>,
    sessions: Arc<SessionStore>,
    config_store: Arc<ConfigStore>,
}

impl ManagementRuntime {
    fn new(
        auth: Arc<AuthState>,
        sessions: Arc<SessionStore>,
        config_store: Arc<ConfigStore>,
    ) -> Self {
        Self {
            auth,
            sessions,
            config_store,
        }
    }

    /// 仅已应用的认证内容变化撤销 session；普通配置变化和单纯文件观测不能回收会话。
    pub(crate) fn reconcile_users(&self, users: &[ResolvedWebUiUser], source_fingerprint: &str) {
        let self_written = self.config_store.observe_reload(source_fingerprint);
        let changed = self.auth.replace(users);
        if changed && !self_written {
            self.sessions.revoke_all();
        }
    }

    pub(crate) fn shutdown(&self) {
        self.sessions.revoke_all();
    }
}
