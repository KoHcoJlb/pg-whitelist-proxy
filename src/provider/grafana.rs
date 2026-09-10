use async_trait::async_trait;
use eyre::{Result, WrapErr, ensure};
use reqwest::{Client, Url};
use serde::Deserialize;

use crate::{config::GrafanaConfig, provider::QueryTemplateProvider};

#[derive(Deserialize)]
struct Target {
    #[serde(default)]
    hide: bool,

    #[serde(rename = "rawSql")]
    raw_sql: Option<String>,
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
struct DashboardResponse {
    dashboard: Dashboard,
}

pub struct GrafanaProvider {
    client: Client,
    base_url: String,
    username: String,
    password: String,
}

impl GrafanaProvider {
    pub fn new(config: &GrafanaConfig) -> Result<Self> {
        let mut base_url = Url::parse(&config.url).wrap_err("invalid Grafana URL")?;
        ensure!(!base_url.cannot_be_a_base(), "Grafana URL cannot be used as a base URL");
        base_url.set_query(None);
        base_url.set_fragment(None);

        Ok(Self {
            client: Client::new(),
            base_url: base_url.as_str().trim_end_matches('/').to_owned(),
            username: config.username.clone(),
            password: config.password.clone(),
        })
    }
}

#[async_trait]
impl QueryTemplateProvider for GrafanaProvider {
    async fn query_templates(&self, org_id: u64, dashboard_uid: &str) -> Result<Vec<String>> {
        ensure!(org_id > 0, "Grafana organization ID must be positive");
        ensure!(
            !dashboard_uid.is_empty()
                && dashboard_uid
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "invalid Grafana dashboard UID"
        );

        let url = format!("{}/api/dashboards/uid/{dashboard_uid}", self.base_url);
        let response = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .header("X-Grafana-Org-Id", org_id.to_string())
            .send()
            .await
            .wrap_err_with(|| format!("failed to fetch Grafana URL {url}"))?
            .error_for_status()
            .wrap_err_with(|| format!("Grafana rejected request for {url}"))?;

        let response: DashboardResponse = response
            .json()
            .await
            .wrap_err_with(|| format!("invalid Grafana response for {url}"))?;

        let mut queries = vec!["-- ping".into()];
        response.dashboard.append_queries(&mut queries);

        Ok(queries)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
        time::timeout,
    };

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    async fn mock_grafana(
        responses: Vec<(String, u64, u16, String)>,
    ) -> (GrafanaProvider, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut provider = GrafanaProvider::new(&GrafanaConfig {
            url: format!("http://{}/grafana/?ignored=yes#fragment", listener.local_addr().unwrap()),
            username: "user".into(),
            password: "pass".into(),
        })
        .unwrap();
        provider.client = Client::builder().no_proxy().timeout(TEST_TIMEOUT).build().unwrap();

        let server = tokio::spawn(async move {
            timeout(TEST_TIMEOUT, async move {
                for (path, org_id, status, body) in responses {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(stream.read_u8().await.unwrap());
                        assert!(request.len() < 16 * 1024, "request headers too large");
                    }
                    let request = String::from_utf8(request).unwrap();
                    assert_eq!(request.lines().next().unwrap(), format!("GET {path} HTTP/1.1"));
                    assert!(request.lines().any(|line| {
                        line.split_once(':').is_some_and(|(name, value)| {
                            name.eq_ignore_ascii_case("authorization")
                                && value.trim() == "Basic dXNlcjpwYXNz"
                        })
                    }), "missing expected Basic authorization: {request}");
                    assert!(request.lines().any(|line| {
                        line.split_once(':').is_some_and(|(name, value)| {
                            name.eq_ignore_ascii_case("x-grafana-org-id")
                                && value.trim() == org_id.to_string()
                        })
                    }), "missing expected organization header: {request}");

                    let response = format!(
                        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
            })
            .await
            .expect("mock Grafana timed out");
        });

        (provider, server)
    }

    #[tokio::test]
    async fn fetches_only_requested_dashboard_with_nested_queries() {
        let (provider, server) = mock_grafana(vec![(
            "/grafana/api/dashboards/uid/first".into(),
            1,
            200,
            r#"{
                "dashboard": {"panels": [{
                    "targets": [
                        {"rawSql":" SELECT 1 "},
                        {"hide":true,"rawSql":"SELECT hidden"},
                        {"rawSql":""},
                        {"rawSql":" \n\t"},
                        {"rawSql":null},
                        {}
                    ],
                    "panels": [{"panels": [{"targets": [
                        {"hide":false,"rawSql":"SELECT 2"},
                        {"hide":true,"rawSql":"SELECT nested_hidden"}
                    ]}]}]
                }]}
            }"#
            .into(),
        )])
        .await;

        let queries = provider.query_templates(1, "first").await.unwrap();

        assert_eq!(queries, ["-- ping", " SELECT 1 ", "SELECT 2"]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn same_uid_is_fetched_for_each_organization() {
        let uid = format!("Legacy_UID-{}", "a".repeat(50));
        let path = format!("/grafana/api/dashboards/uid/{uid}");
        let (provider, server) = mock_grafana(vec![
            (path.clone(), 1, 200, r#"{"dashboard":{}}"#.into()),
            (
                path,
                2,
                200,
                r#"{"dashboard":{"panels":[{"targets":[{"rawSql":"SELECT new"}]}]}}"#.into(),
            ),
        ])
        .await;

        assert_eq!(provider.query_templates(1, &uid).await.unwrap(), ["-- ping"]);
        assert_eq!(provider.query_templates(2, &uid).await.unwrap(), ["-- ping", "SELECT new"]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dashboard_errors_propagate() {
        let path = "/grafana/api/dashboards/uid/broken";
        for (status, body, message) in [
            (503, "{}", "Grafana rejected request"),
            (404, "{}", "Grafana rejected request"),
            (401, "{}", "Grafana rejected request"),
            (200, "not json", "invalid Grafana response"),
            (200, "{}", "invalid Grafana response"),
        ] {
            let (provider, server) =
                mock_grafana(vec![(path.into(), 1, status, body.into())]).await;

            let error = provider.query_templates(1, "broken").await.unwrap_err();

            let message_with_url = error.to_string();
            assert!(message_with_url.contains(message), "{error:?}");
            assert!(message_with_url.contains(path), "{error:?}");
            if status != 200 {
                assert!(format!("{error:?}").contains(&status.to_string()), "{error:?}");
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_ids_are_rejected_before_sending_requests() {
        let (provider, server) = mock_grafana(vec![]).await;
        server.await.unwrap();

        for uid in ["", "/", "a/b", "a?b", "a#b", "..", "a/../b", "%2F", "a b", "\u{e9}"] {
            let error = provider.query_templates(1, uid).await.unwrap_err();

            assert!(
                error.to_string().contains("invalid Grafana dashboard UID"),
                "{uid}: {error:?}"
            );
        }

        let error = provider.query_templates(0, "valid").await.unwrap_err();

        assert!(error.to_string().contains("organization ID must be positive"), "{error:?}");
    }
}
