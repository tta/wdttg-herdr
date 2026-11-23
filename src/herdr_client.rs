use serde::Deserialize;
use std::process::Command;

#[derive(Debug, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Workspace {
    #[serde(rename = "workspace_id")]
    pub id: String,
    pub label: Option<String>,
}

pub struct HerdrClient {
    bin_path: String,
}

impl HerdrClient {
    pub fn new() -> Self {
        let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
        Self { bin_path: bin }
    }

    pub fn poll(&self) -> Result<Snapshot, HerdrError> {
        let output = Command::new(&self.bin_path)
            .args(["api", "snapshot"])
            .output()
            .map_err(|e| HerdrError::Spawn(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(HerdrError::NonZeroExit(stderr.to_string()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // The CLI wraps the response in {id, result: {snapshot: {...}}}
        let parsed: serde_json::Value =
            serde_json::from_str(&stdout).map_err(|e| HerdrError::Parse(e.to_string()))?;
        let snapshot_val = parsed
            .get("result")
            .and_then(|r| r.get("snapshot"))
            .ok_or_else(|| HerdrError::Parse("missing result.snapshot".to_string()))?;
        serde_json::from_value::<Snapshot>(snapshot_val.clone())
            .map_err(|e| HerdrError::Parse(format!("failed to parse snapshot: {e}")))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    #[error("failed to spawn herdr: {0}")]
    Spawn(String),
    #[error("herdr exited non-zero: {0}")]
    NonZeroExit(String),
    #[error("{0}")]
    Parse(String),
}
