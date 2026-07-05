use arti_client::{config::TorClientConfigBuilder, DataStream, TorClient};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tor_rtcompat::PreferredRuntime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorState {
    Stopped,
    Bootstrapping,
    Ready,
    Error,
}

#[derive(Clone)]
struct Inner {
    client: Arc<Mutex<Option<TorClient<PreferredRuntime>>>>,
    state: Arc<Mutex<TorState>>,
    data_dir: PathBuf,
}

#[derive(Clone)]
pub struct TorService {
    inner: Inner,
}

impl TorService {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            inner: Inner {
                client: Arc::new(Mutex::new(None)),
                state: Arc::new(Mutex::new(TorState::Stopped)),
                data_dir,
            },
        }
    }

    pub async fn start(&self) -> Result<(), String> {
        let mut state = self.inner.state.lock().await;
        if *state == TorState::Ready || *state == TorState::Bootstrapping {
            log::debug!("tor start skipped, already {:?}", *state);
            return Ok(());
        }
        *state = TorState::Bootstrapping;
        drop(state);

        let state_dir = self.inner.data_dir.join("tor-state");
        let cache_dir = self.inner.data_dir.join("tor-cache");
        log::info!(
            "tor starting; data_dir={:?} state_dir={:?} cache_dir={:?}",
            self.inner.data_dir,
            state_dir,
            cache_dir
        );

        for dir in [&state_dir, &cache_dir] {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::error!("tor create dir {:?} failed: {e}", dir);
                return Err(format!("tor create dir {:?}: {e}", dir));
            }
        }

        let mut builder = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir);
        // Android stores app data under /data/data/<pkg>/files, which fs-mistrust
        // rejects as "accessible by other users" and aborts bootstrap with a config
        // error. Trust the app's private sandbox so Arti can use its state/cache dirs.
        // Only Android needs this: its private app sandbox (/data/data/<pkg>) is
        // flagged by fs-mistrust as "accessible by others". On desktop, keep Arti's
        // strict filesystem permission checks active.
        #[cfg(target_os = "android")]
        builder.storage().permissions().dangerously_trust_everyone();
        // Arti rejects .onion targets by default ("allow_onion_addrs disabled");
        // the onion-service-client feature only compiles the code in. Opt in here
        // so connecting to the Umbrel tapd .onion is permitted.
        builder.address_filter().allow_onion_addrs(true);
        let config = builder.build().map_err(|e| {
            log::error!("tor build config failed: {e}");
            format!("tor build config: {e}")
        })?;

        match TorClient::create_bootstrapped(config).await {
            Ok(client) => {
                log::info!("tor bootstrapped successfully");
                *self.inner.state.lock().await = TorState::Ready;
                *self.inner.client.lock().await = Some(client);
                Ok(())
            }
            Err(e) => {
                log::error!("tor bootstrap failed: {e}");
                *self.inner.state.lock().await = TorState::Error;
                *self.inner.client.lock().await = None;
                Err(format!("tor bootstrap: {e}"))
            }
        }
    }

    pub async fn stop(&self) {
        *self.inner.client.lock().await = None;
        *self.inner.state.lock().await = TorState::Stopped;
    }

    pub async fn state(&self) -> TorState {
        *self.inner.state.lock().await
    }

    pub async fn connect(&self, host: &str, port: u16) -> Result<DataStream, String> {
        log::info!("tor connect request: {host}:{port}");
        let client = {
            let guard = self.inner.client.lock().await;
            guard
                .as_ref()
                .ok_or_else(|| "tor client is not running".to_string())?
                .clone()
        };
        let result = client.connect((host, port)).await;
        match &result {
            Ok(_) => log::info!("tor connected to {host}:{port}"),
            Err(e) => log::error!("tor connect to {host}:{port} failed: {e}"),
        }
        result.map_err(|e| format!("tor connect: {e}"))
    }
}
