use anyhow::Context;
use anyhow::Result;
use codex_bridge_core::BridgeState;
use codex_bridge_core::BridgeStateStore;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

pub(crate) struct JsonFileStateStore {
    path: PathBuf,
}

impl JsonFileStateStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl BridgeStateStore for JsonFileStateStore {
    fn load_state(&self) -> Result<BridgeState> {
        match fs::read_to_string(self.path()) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parse telegram bridge state {}", self.path().display())),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(BridgeState::default()),
            Err(err) => Err(err).with_context(|| format!("read state {}", self.path().display())),
        }
    }

    fn persist_state(&self, state: &BridgeState) -> Result<()> {
        if let Some(parent) = self.path().parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create state directory {}", parent.display()))?;
        }

        let payload = serde_json::to_string_pretty(state).context("serialize state")?;
        fs::write(self.path(), payload)
            .with_context(|| format!("write state {}", self.path().display()))?;
        Ok(())
    }
}

pub(crate) fn default_state_path(config_path: &Path) -> PathBuf {
    if let Some(parent) = config_path.parent() {
        return parent.join("state.json");
    }
    PathBuf::from("telegram-state.json")
}
