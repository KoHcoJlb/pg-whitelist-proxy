use async_trait::async_trait;
use eyre::Result;

#[async_trait]
pub trait QueryTemplateProvider: Send + Sync {
    async fn query_templates(&self) -> Result<Vec<String>>;
}

pub mod grafana;
pub use grafana::GrafanaProvider;
