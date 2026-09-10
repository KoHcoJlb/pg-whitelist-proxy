use async_trait::async_trait;
use eyre::{Result, WrapErr, ensure};
use reqwest::{Client, Url};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{config::GrafanaConfig, provider::QueryTemplateProvider};

const SEARCH_PAGE_SIZE: usize = 1000;

#[derive(Deserialize)]
struct DashboardSearchResult {
    uid: String,
}

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

    async fn get<T: DeserializeOwned>(&self, url: String) -> Result<T> {
        let response = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .wrap_err_with(|| format!("failed to fetch Grafana URL {url}"))?
            .error_for_status()
            .wrap_err_with(|| format!("Grafana rejected request for {url}"))?;

        response.json().await.wrap_err_with(|| format!("invalid Grafana response for {url}"))
    }

    async fn dashboard_uids(&self) -> Result<Vec<String>> {
        let mut uids = Vec::new();

        for page in 1.. {
            let results: Vec<DashboardSearchResult> = self
                .get(format!(
                    "{}/api/search?type=dash-db&limit={SEARCH_PAGE_SIZE}&page={page}",
                    self.base_url
                ))
                .await?;

            let is_last_page = results.len() < SEARCH_PAGE_SIZE;
            uids.extend(results.into_iter().map(|result| result.uid));
            if is_last_page {
                break;
            }
        }

        Ok(uids)
    }

    async fn dashboard(&self, uid: &str) -> Result<Dashboard> {
        let response: DashboardResponse =
            self.get(format!("{}/api/dashboards/uid/{uid}", self.base_url)).await?;
        Ok(response.dashboard)
    }
}

#[async_trait]
impl QueryTemplateProvider for GrafanaProvider {
    async fn query_templates(&self) -> Result<Vec<String>> {
        let mut queries = vec!["-- ping".into()];

        for uid in self.dashboard_uids().await? {
            self.dashboard(&uid).await?.append_queries(&mut queries);
        }

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
    const SEARCH: &str = "/grafana/api/search?type=dash-db&limit=1000&page=1";

    async fn mock_grafana(
        responses: Vec<(&'static str, u16, String)>,
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
                for (path, status, body) in responses {
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
    async fn discovers_and_fetches_nested_queries() {
        let (provider, server) = mock_grafana(vec![
            (SEARCH, 200, r#"[{"uid":"first"},{"uid":"second"}]"#.into()),
            (
                "/grafana/api/dashboards/uid/first",
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
            ),
            (
                "/grafana/api/dashboards/uid/second",
                200,
                r#"{"dashboard":{"panels":[{"targets":[{"rawSql":"SELECT 3"}]}]}}"#.into(),
            ),
        ])
        .await;

        let queries = provider.query_templates().await.unwrap();

        assert_eq!(queries, ["-- ping", " SELECT 1 ", "SELECT 2", "SELECT 3"]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn paginates_until_a_short_search_page() {
        let mut expected: Vec<String> = (0..1000).map(|index| format!("uid-{index}")).collect();
        let first_page = format!(
            "[{}]",
            expected
                .iter()
                .map(|uid| format!(r#"{{"uid":"{uid}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        expected.extend(["last-a".into(), "last-b".into()]);
        let (provider, server) = mock_grafana(vec![
            (SEARCH, 200, first_page),
            (
                "/grafana/api/search?type=dash-db&limit=1000&page=2",
                200,
                r#"[{"uid":"last-a"},{"uid":"last-b"}]"#.into(),
            ),
        ])
        .await;

        let uids = provider.dashboard_uids().await.unwrap();

        assert_eq!(uids, expected);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_search_returns_ping_and_next_call_rediscovers() {
        let (provider, server) = mock_grafana(vec![
            (SEARCH, 200, "[]".into()),
            (SEARCH, 200, r#"[{"uid":"new"}]"#.into()),
            (
                "/grafana/api/dashboards/uid/new",
                200,
                r#"{"dashboard":{"panels":[{"targets":[{"rawSql":"SELECT new"}]}]}}"#.into(),
            ),
        ])
        .await;

        assert_eq!(provider.query_templates().await.unwrap(), ["-- ping"]);
        assert_eq!(provider.query_templates().await.unwrap(), ["-- ping", "SELECT new"]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn search_and_dashboard_errors_propagate() {
        for (path, status, body, message) in [
            (SEARCH, 503, "[]", "Grafana rejected request"),
            (SEARCH, 200, "not json", "invalid Grafana response"),
            ("/grafana/api/dashboards/uid/broken", 404, "{}", "Grafana rejected request"),
        ] {
            let mut responses = Vec::new();
            if path != SEARCH {
                responses.push((SEARCH, 200, r#"[{"uid":"broken"}]"#.into()));
            }
            responses.push((path, status, body.into()));
            let (provider, server) = mock_grafana(responses).await;

            let error = provider.query_templates().await.unwrap_err();

            let message_with_url = error.to_string();
            assert!(message_with_url.contains(message), "{error:?}");
            assert!(message_with_url.contains(path), "{error:?}");
            if status != 200 {
                assert!(format!("{error:?}").contains(&status.to_string()), "{error:?}");
            }
            server.await.unwrap();
        }
    }
}
