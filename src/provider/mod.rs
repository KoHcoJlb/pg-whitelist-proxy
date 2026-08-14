pub mod grafana;

use async_trait::async_trait;
use eyre::Result;
pub use grafana::GrafanaProvider;

#[async_trait]
pub trait QueryTemplateProvider: Send + Sync {
    async fn query_templates(&self) -> Result<Vec<String>>;
}
