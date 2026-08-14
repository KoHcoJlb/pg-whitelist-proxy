use async_trait::async_trait;
use eyre::{Result, WrapErr, eyre};
use reqwest::{Client, Url};
use serde::Deserialize;

use crate::{config::GrafanaConfig, provider::QueryTemplateProvider};

pub struct GrafanaProvider {
    client: Client,
    base_url: Url,
    token: String,
    dashboard_uids: Vec<String>,
}

impl GrafanaProvider {
    pub fn new(config: &GrafanaConfig) -> Result<Self> {
        let mut base_url = Url::parse(&config.url).wrap_err("invalid Grafana URL")?;
        base_url.set_query(None);
        base_url.set_fragment(None);

        Ok(Self {
            client: Client::new(),
            base_url,
            token: config.token.clone(),
            dashboard_uids: config.dashboard_uids.clone(),
        })
    }

    fn dashboard_url(&self, uid: &str) -> Result<Url> {
        let mut url = self.base_url.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| eyre!("Grafana URL cannot be used as a base URL"))?;
        path.pop_if_empty();
        path.extend(["api", "dashboards", "uid", uid]);
        drop(path);
        Ok(url)
    }

    async fn dashboard(&self, uid: &str) -> Result<Dashboard> {
        let response = self
            .client
            .get(self.dashboard_url(uid)?)
            .bearer_auth(&self.token)
            .send()
            .await
            .wrap_err_with(|| format!("failed to fetch Grafana dashboard {uid:?}"))?
            .error_for_status()
            .wrap_err_with(|| format!("Grafana rejected dashboard request for {uid:?}"))?;

        let response: DashboardResponse = response
            .json()
            .await
            .wrap_err_with(|| format!("invalid Grafana dashboard response for {uid:?}"))?;
        Ok(response.dashboard)
    }
}

#[async_trait]
impl QueryTemplateProvider for GrafanaProvider {
    async fn query_templates(&self) -> Result<Vec<String>> {
        let mut queries = vec!["-- ping".into()];

        for uid in &self.dashboard_uids {
            self.dashboard(uid).await?.append_queries(&mut queries);
        }

        Ok(queries)
    }
}

#[derive(Deserialize)]
struct DashboardResponse {
    dashboard: Dashboard,
}

#[derive(Deserialize)]
struct Dashboard {
    #[serde(default)]
    panels: Vec<Panel>,
}

impl Dashboard {
    fn append_queries(&self, queries: &mut Vec<String>) {
        for panel in &self.panels {
            panel.append_queries(queries);
        }
    }
}

#[derive(Deserialize)]
struct Panel {
    #[serde(default)]
    panels: Vec<Panel>,

    #[serde(default)]
    targets: Vec<Target>,
}

impl Panel {
    fn append_queries(&self, queries: &mut Vec<String>) {
        for target in &self.targets {
            if target.hide {
                continue;
            }

            if let Some(query) = &target.raw_sql
                && !query.trim().is_empty()
            {
                queries.push(query.clone());
            }
        }

        for panel in &self.panels {
            panel.append_queries(queries);
        }
    }
}

#[derive(Deserialize)]
struct Target {
    #[serde(default)]
    hide: bool,

    #[serde(rename = "rawSql")]
    raw_sql: Option<String>,
}
