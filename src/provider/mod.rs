use async_trait::async_trait;
use eyre::Result;

#[async_trait]
pub trait QueryTemplateProvider: Send + Sync {
    async fn query_templates(&self, org_id: u64, dashboard_uid: &str) -> Result<Vec<String>>;
}

pub mod grafana;
pub use grafana::GrafanaProvider;
