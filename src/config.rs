use std::collections::HashMap;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub proxy: ProxyConfig,

    pub grafana: GrafanaConfig,

    #[serde(default)]
    pub variable_templates: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProxyConfig {
    pub listen_addr: String,
    pub server_addr: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GrafanaConfig {
    pub url: String,
    #[serde(default)]
    pub token: String,
    pub dashboard_uids: Vec<String>,
}
