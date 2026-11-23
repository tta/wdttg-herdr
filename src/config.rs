use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bridge_threshold")]
    pub bridge_threshold_seconds: u64,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
    #[serde(default = "default_task_name_template")]
    pub task_name_template: String,
    #[serde(default = "default_rename_timeout")]
    pub rename_timeout_seconds: u64,
}

fn default_bridge_threshold() -> u64 {
    180
}
fn default_poll_interval() -> u64 {
    3
}
fn default_task_name_template() -> String {
    "herdr:{label}".to_string()
}
fn default_rename_timeout() -> u64 {
    60
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bridge_threshold_seconds: default_bridge_threshold(),
            poll_interval_seconds: default_poll_interval(),
            task_name_template: default_task_name_template(),
            rename_timeout_seconds: default_rename_timeout(),
        }
    }
}

impl Config {
    pub fn load(config_dir: &std::path::Path) -> Self {
        let path = config_dir.join("config.toml");
        if path.exists() {
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| toml::from_str::<Config>(&s).ok())
            {
                Some(cfg) => return cfg,
                None => {
                    eprintln!("Warning: failed to parse {}, using defaults", path.display());
                }
            }
        }
        Self::default()
    }

    pub fn derive_task_name(&self, label: &str, cwd: Option<&str>) -> String {
        let effective = if label.is_empty() {
            cwd.map(|c| {
                c.rsplit('/')
                    .next()
                    .unwrap_or(c)
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "workspace".to_string())
        } else {
            label.to_string()
        };
        self.task_name_template.replace("{label}", &effective)
    }
}
