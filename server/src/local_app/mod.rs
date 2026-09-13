//! Exposes the local desktop application integration.
mod account;
mod ca;
mod process;
mod proxy;
mod remote_ssh;
mod settings;

use std::{net::SocketAddr, sync::Arc};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    store::{Store, TabMode, TabSettings},
    Error, Result,
};

use self::{ca::CaManager, proxy::ProxyRuntime};

pub(crate) fn proxy_host_allowed(host: &str) -> bool {
    proxy::is_cursor_host(host)
}

pub(crate) fn request_uses_local_cursor_token(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(account::is_local_cursor_authorization)
}

#[cfg(test)]
pub(crate) fn local_cursor_authorization() -> String {
    format!("Bearer {}", account::local_token().unwrap())
}

fn integration_prerequisites_ready(ca: &CaState, backend_ready: bool) -> bool {
    matches!(ca, CaState::Ready) && backend_ready
}

fn should_terminate_cursor(explicit: bool, settings_applied: bool) -> bool {
    explicit && !settings_applied
}

fn integration_state(proxy_running: bool, settings_applied: bool) -> IntegrationState {
    match (proxy_running, settings_applied) {
        (false, false) => IntegrationState::Disabled,
        (true, true) => IntegrationState::Enabled,
        _ => IntegrationState::Degraded,
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaState {
    Missing,
    Untrusted,
    Ready,
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    Disabled,
    Enabled,
    Degraded,
}

#[derive(Clone, Debug, Serialize)]
pub struct CursorHarnessStatus {
    pub platform: &'static str,
    pub ca: CaState,
    pub configured_models: usize,
    pub enabled_models: usize,
    pub integration: IntegrationState,
    pub settings_applied: bool,
    pub proxy_url: Option<String>,
    pub ca_install_command: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SetEnabled {
    pub enabled: bool,
}

#[derive(Clone)]
pub struct CursorHarness {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    ca: CaManager,
    ca_initialization: Mutex<()>,
    backend_addr: RwLock<Option<SocketAddr>>,
    tab_mode: Arc<RwLock<TabMode>>,
    proxy: Mutex<ProxyRuntime>,
}

impl CursorHarness {
    pub fn new(store: Store) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                ca: CaManager::managed()?,
                ca_initialization: Mutex::new(()),
                backend_addr: RwLock::new(None),
                tab_mode: Arc::new(RwLock::new(TabMode::default())),
                proxy: Mutex::new(ProxyRuntime::default()),
            }),
        })
    }

    pub fn set_backend_addr(&self, addr: SocketAddr) {
        *self.inner.backend_addr.write() = Some(addr);
    }

    pub async fn proxy_port(&self) -> Option<u16> {
        self.inner.proxy.lock().await.port()
    }

    pub async fn cleanup_stale_settings(&self) -> Result<()> {
        remote_ssh::disable()?;
        settings::clear_stale_proxy_settings()
    }

    /// Observes the integration without starting or stopping it.
    pub async fn status(&self) -> Result<CursorHarnessStatus> {
        let models = self.inner.store.models().await?;
        let configured_models = models.len();
        let enabled_models = configured_models;
        let ca = self.inner.ca.state()?;
        let proxy = self.inner.proxy.lock().await;
        let proxy_url = proxy.url();
        let settings_applied = proxy_url
            .as_deref()
            .map(remote_ssh::settings_match)
            .transpose()?
            .unwrap_or(false);
        let integration = integration_state(proxy.running(), settings_applied);
        Ok(CursorHarnessStatus {
            platform: std::env::consts::OS,
            ca,
            configured_models,
            enabled_models,
            integration,
            settings_applied,
            proxy_url,
            ca_install_command: self.inner.ca.install_command(),
        })
    }

    /// Restores the integration once the server has established its backend address.
    pub async fn restore_on_startup(&self) {
        let ca = match self.inner.ca.state() {
            Ok(ca) => ca,
            Err(error) => {
                tracing::warn!(%error, "failed to read CA state during startup");
                return;
            }
        };
        let restore = match self.inner.store.cursor_takeover_enabled().await {
            Ok(restore) => restore,
            Err(error) => {
                tracing::warn!(%error, "failed to read Cursor integration preference during startup");
                return;
            }
        };
        if restore && integration_prerequisites_ready(&ca, self.inner.backend_addr.read().is_some())
        {
            if let Err(error) = self.enable(false).await {
                tracing::warn!(%error, "failed to restore Cursor harness during startup");
            }
        }
    }

    pub async fn initialize_ca(&self) -> Result<CursorHarnessStatus> {
        let _initialization = self.inner.ca_initialization.lock().await;
        let manager = self.inner.ca.clone();
        tokio::task::spawn_blocking(move || manager.initialize_local())
            .await
            .map_err(|error| Error::Store(format!("CA initialization task failed: {error}")))??;
        self.status().await
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<CursorHarnessStatus> {
        if enabled {
            self.enable(true).await?;
        } else {
            self.disable().await?;
        }
        self.inner
            .store
            .set_cursor_takeover_enabled(enabled)
            .await?;
        self.status().await
    }

    pub async fn set_tab_settings(&self, settings: TabSettings) -> Result<TabSettings> {
        let saved = self.inner.store.set_tab_settings(settings).await?;
        *self.inner.tab_mode.write() = saved.mode;
        Ok(saved)
    }

    async fn enable(&self, explicit: bool) -> Result<()> {
        if !matches!(self.inner.ca.state()?, CaState::Ready) {
            return Err(Error::Config(
                "initialize and trust the CA before enabling Cursor".into(),
            ));
        }
        let backend_addr = self
            .inner
            .backend_addr
            .read()
            .ok_or_else(|| Error::Config("desktop management server is not ready".into()))?;
        let mut proxy = self.inner.proxy.lock().await;
        let settings_applied = proxy
            .url()
            .as_deref()
            .map(remote_ssh::settings_match)
            .transpose()?
            .unwrap_or(false);
        if should_terminate_cursor(explicit, settings_applied) {
            process::terminate_cursor().await?;
        }
        if proxy.running() {
            if let (Some(url), Some(port), Some(skill_sync)) =
                (proxy.url(), proxy.port(), proxy.skill_sync())
            {
                apply_cursor_configuration(
                    &url,
                    port,
                    &self.inner.ca.certificate_pem()?,
                    &skill_sync,
                )
                .await?;
            }
            return Ok(());
        }
        let certificate_pem = self.inner.ca.certificate_pem()?;
        let ca = self.inner.ca.load()?;
        let skill_sync = remote_ssh::SkillSyncServer::new()?;
        let requested_port = self.inner.store.port_settings().await?.proxy_port;
        *self.inner.tab_mode.write() = self.inner.store.tab_settings().await?.mode;
        let (url, actual_port) = proxy
            .start(
                backend_addr,
                ca,
                requested_port,
                self.inner.tab_mode.clone(),
                skill_sync.clone(),
            )
            .await?;
        if let Err(error) = self.inner.store.set_proxy_port(actual_port).await {
            proxy.stop().await;
            return Err(error);
        }
        if let Err(error) =
            apply_cursor_configuration(&url, actual_port, &certificate_pem, &skill_sync).await
        {
            proxy.stop().await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn disable(&self) -> Result<()> {
        remote_ssh::disable()?;
        self.inner.proxy.lock().await.stop().await;
        Ok(())
    }
}

async fn apply_cursor_configuration(
    proxy_url: &str,
    proxy_port: u16,
    certificate_pem: &str,
    skill_sync: &remote_ssh::SkillSyncServer,
) -> Result<()> {
    account::ensure_local_account().await?;
    remote_ssh::enable(proxy_url, proxy_port, certificate_pem, skill_sync)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_does_not_write_settings_start_proxy_or_create_ca() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect("sqlite::memory:").await.unwrap();
        store.set_cursor_takeover_enabled(false).await.unwrap();
        let harness = CursorHarness {
            inner: Arc::new(Inner {
                store: store.clone(),
                ca: CaManager::at(directory.path().join("ca")),
                ca_initialization: Mutex::new(()),
                backend_addr: RwLock::new(None),
                tab_mode: Arc::new(RwLock::new(TabMode::default())),
                proxy: Mutex::new(ProxyRuntime::default()),
            }),
        };
        let before: Vec<(String, String, i64)> = sqlx::query_as("SELECT setting_key, value_json, updated_at_ms FROM service_settings ORDER BY setting_key").fetch_all(store.pool()).await.unwrap();
        for _ in 0..2 {
            let status = harness.status().await.unwrap();
            assert_eq!(status.integration, IntegrationState::Disabled);
            assert!(!status.settings_applied);
            assert!(status.proxy_url.is_none());
        }
        harness.restore_on_startup().await;
        let after: Vec<(String, String, i64)> = sqlx::query_as("SELECT setting_key, value_json, updated_at_ms FROM service_settings ORDER BY setting_key").fetch_all(store.pool()).await.unwrap();
        assert_eq!(before, after);
        assert!(!directory.path().join("ca").exists());
        assert!(harness.proxy_port().await.is_none());
    }

    #[test]
    fn cursor_termination_only_precedes_explicit_configuration_changes() {
        assert!(should_terminate_cursor(true, false));
        assert!(!should_terminate_cursor(true, true));
        assert!(!should_terminate_cursor(false, false));
        assert!(!should_terminate_cursor(false, true));
    }

    #[test]
    fn status_classifies_integration_without_changing_it() {
        assert_eq!(integration_state(false, false), IntegrationState::Disabled);
        assert_eq!(integration_state(true, true), IntegrationState::Enabled);
        assert_eq!(integration_state(true, false), IntegrationState::Degraded);
        assert_eq!(integration_state(false, true), IntegrationState::Degraded);
    }

    #[test]
    fn startup_restore_requires_both_ca_and_backend() {
        assert!(integration_prerequisites_ready(&CaState::Ready, true));
        assert!(!integration_prerequisites_ready(&CaState::Ready, false));
        assert!(!integration_prerequisites_ready(&CaState::Missing, true));
        assert!(!integration_prerequisites_ready(&CaState::Untrusted, true));
        assert!(!integration_prerequisites_ready(&CaState::Invalid, true));
    }
}
