use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use eyre::{OptionExt, Result, WrapErr, ensure, eyre};
use futures::{SinkExt, StreamExt};
use moka::future::Cache;
use pgwire::{
    api::{ClientInfo, DefaultClient, PgWireConnectionState},
    messages::{
        PgWireBackendMessage, PgWireFrontendMessage, ProtocolVersion, SslNegotiationMetaMessage,
        response::{ErrorResponse, GssEncResponse, ReadyForQuery, SslResponse, TransactionStatus},
    },
    tokio::{client::PgWireMessageClientCodec, server::PgWireMessageServerCodec},
};
use tokio::{
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_util::codec::Framed;
use tracing::{Instrument, error, info, info_span, trace, warn};

use crate::{
    config::Config,
    provider::QueryTemplateProvider,
    template::matcher::{query::QueryTemplateMatcher, variable::VariableTemplateMatcher},
};

const WHITELIST_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const WHITELIST_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const WHITELIST_CACHE_CAPACITY: u64 = 1024;
const GRAFANA_ORG_ID_PARAMETER: &str = "grafana.org_id";
const GRAFANA_DASHBOARD_UID_PARAMETER: &str = "grafana.dashboard_uid";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DashboardScope {
    org_id: u64,
    dashboard_uid: String,
}

impl DashboardScope {
    fn from_parameters(parameters: &BTreeMap<String, String>) -> Result<Option<Self>> {
        let Some(org_id) = parameters.get(GRAFANA_ORG_ID_PARAMETER).filter(|id| !id.is_empty())
        else {
            return Ok(None);
        };
        let Some(dashboard_uid) =
            parameters.get(GRAFANA_DASHBOARD_UID_PARAMETER).filter(|uid| !uid.is_empty())
        else {
            return Ok(None);
        };

        let org_id = org_id.parse::<u64>().wrap_err("invalid grafana.org_id")?;
        ensure!(org_id > 0, "grafana.org_id must be positive");
        ensure!(
            dashboard_uid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "invalid grafana.dashboard_uid"
        );

        Ok(Some(Self { org_id, dashboard_uid: dashboard_uid.clone() }))
    }
}

#[derive(Default)]
struct QueryWhitelist {
    named: HashMap<String, QueryTemplateMatcher>,
    unnamed: Vec<QueryTemplateMatcher>,
}

impl QueryWhitelist {
    fn len(&self) -> usize {
        self.named.len() + self.unnamed.len()
    }
}

async fn fetch_whitelist(
    provider: &dyn QueryTemplateProvider,
    variable_templates: &Arc<HashMap<String, VariableTemplateMatcher>>, scope: &DashboardScope,
) -> Result<QueryWhitelist> {
    let query_templates = provider.query_templates(scope.org_id, &scope.dashboard_uid).await?;
    let mut whitelist = QueryWhitelist::default();

    for query_template in query_templates {
        let matcher = QueryTemplateMatcher::parse(&query_template, variable_templates.clone())
            .wrap_err("invalid query template")?;

        if let Some(name) = matcher.name().map(str::to_owned) {
            if whitelist.named.contains_key(&name) {
                warn!(query_name = %name, "skipping duplicate named query template");
                continue;
            }
            whitelist.named.insert(name, matcher);
        } else {
            whitelist.unnamed.push(matcher);
        }
    }

    Ok(whitelist)
}

fn query_is_allowed(whitelist: &QueryWhitelist, query: &str) -> Result<()> {
    if let Some(name) = QueryTemplateMatcher::query_name(query) {
        let res = whitelist.named.get(name).ok_or_eyre("named query not found")?.match_query(query);
        if let Err(err) = &res {
            warn!(name, ?err, "named query rejected by whitelist");
        }
        return res.map_err(Into::into);
    }

    if whitelist.unnamed.iter().any(|matcher| matcher.match_query(query).is_ok()) {
        Ok(())
    } else {
        warn!(query, "no unnamed query matches");
        Err(eyre!("no unnamed query matches"))
    }
}

struct WhitelistCache {
    provider: Arc<dyn QueryTemplateProvider>,
    variable_templates: Arc<HashMap<String, VariableTemplateMatcher>>,
    dashboards: Cache<DashboardScope, Arc<Result<QueryWhitelist>>>,
}

impl WhitelistCache {
    fn new(
        provider: Arc<dyn QueryTemplateProvider>,
        variable_templates: Arc<HashMap<String, VariableTemplateMatcher>>,
    ) -> Self {
        Self {
            provider,
            variable_templates,
            dashboards: Cache::builder()
                .max_capacity(WHITELIST_CACHE_CAPACITY)
                .time_to_live(WHITELIST_REFRESH_INTERVAL)
                .build(),
        }
    }

    async fn query_is_allowed(&self, scope: Option<&DashboardScope>, query: &str) -> Result<()> {
        if query.trim() == "-- ping" {
            return Ok(());
        }

        let scope = scope.ok_or_eyre("grafana.org_id and grafana.dashboard_uid are required")?;
        let cached = self
            .dashboards
            .get_with_by_ref(scope, async {
                let result = timeout(
                    WHITELIST_FETCH_TIMEOUT,
                    fetch_whitelist(&*self.provider, &self.variable_templates, scope),
                )
                .await
                .wrap_err("dashboard whitelist fetch timed out")
                .and_then(|result| result)
                .wrap_err_with(|| format!("failed to load query whitelist for {scope:?}"));
                if let Ok(whitelist) = &result {
                    info!(?scope, queries = whitelist.len(), "dashboard query whitelist refreshed");
                }

                // Cache failures too so concurrent queries do not repeatedly retry a failing dashboard.
                Arc::new(result)
            })
            .await;
        let whitelist = cached.as_ref().as_ref().map_err(|err| eyre!("{err:?}"))?;

        query_is_allowed(whitelist, query)
    }
}

async fn send_access_denied(
    client: &mut Framed<TcpStream, PgWireMessageServerCodec<()>>,
) -> Result<()> {
    let response = ErrorResponse::new(vec![
        (b'S', "ERROR".into()),
        (b'C', "42501".into()),
        (b'M', "query is not permitted by the whitelist".into()),
    ]);

    client
        .send(PgWireBackendMessage::ErrorResponse(response))
        .await
        .wrap_err("failed to send access denied response")
}

async fn proxy_connection(
    client_socket: TcpStream, client_addr: SocketAddr, server_addr: &str,
    cache: Arc<WhitelistCache>,
) -> Result<()> {
    client_socket.set_nodelay(true).wrap_err("failed to configure client connection")?;

    let server_socket = TcpStream::connect(server_addr)
        .await
        .wrap_err_with(|| format!("failed to connect to PostgreSQL server at {server_addr}"))?;
    server_socket.set_nodelay(true).wrap_err("failed to configure server connection")?;

    let client_info = DefaultClient::<()>::new(client_addr, false);
    let mut client = Framed::new(client_socket, PgWireMessageServerCodec::new(client_info));
    let mut server = Framed::new(server_socket, PgWireMessageClientCodec::default());
    let mut transaction_status = TransactionStatus::Idle;
    let mut rejected_extended_query = false;
    let mut is_admin = false;
    let mut dashboard_scope = None;

    info!("connected to PostgreSQL server");

    loop {
        tokio::select! {
            message = client.next() => {
                let Some(message) = message else {
                    break;
                };
                let message = message.wrap_err("failed to decode client message")?;

                trace!(client = ?message);

                match &message {
                    PgWireFrontendMessage::SslNegotiation(
                        SslNegotiationMetaMessage::PostgresSsl(_),
                    ) => {
                        client
                            .send(PgWireBackendMessage::SslResponse(SslResponse::Refuse))
                            .await
                            .wrap_err("failed to refuse client SSL")?;
                        continue;
                    }
                    PgWireFrontendMessage::SslNegotiation(
                        SslNegotiationMetaMessage::PostgresGss(_),
                    ) => {
                        client
                            .send(PgWireBackendMessage::GssEncResponse(GssEncResponse::Refuse))
                            .await
                            .wrap_err("failed to refuse client GSS encryption")?;
                        continue;
                    }
                    PgWireFrontendMessage::SslNegotiation(SslNegotiationMetaMessage::None) => {
                        client.set_state(PgWireConnectionState::AwaitingStartup);
                        continue;
                    }
                    PgWireFrontendMessage::Startup(startup) => {
                        let protocol_version = ProtocolVersion::from_version_number(
                            startup.protocol_number_major,
                            startup.protocol_number_minor,
                        )
                        .ok_or_else(|| {
                            eyre!(
                                "unsupported PostgreSQL protocol version {}.{}",
                                startup.protocol_number_major,
                                startup.protocol_number_minor,
                            )
                        })?;

                        client.set_protocol_version(protocol_version);
                        client.set_state(PgWireConnectionState::AuthenticationInProgress);
                        is_admin = startup
                            .parameters
                            .get("grafana.role")
                            .is_some_and(|role| role == "Admin");
                        dashboard_scope = if is_admin {
                            None
                        } else {
                            DashboardScope::from_parameters(&startup.parameters)?
                        };
                    }
                    PgWireFrontendMessage::Sync(_) if rejected_extended_query => {
                        rejected_extended_query = false;
                        client
                            .send(PgWireBackendMessage::ReadyForQuery(ReadyForQuery::new(
                                transaction_status,
                            )))
                            .await
                            .wrap_err("failed to finish rejected extended query")?;
                        client.set_state(PgWireConnectionState::ReadyForQuery);
                        continue;
                    }
                    _ if rejected_extended_query => continue,
                    PgWireFrontendMessage::Query(_) | PgWireFrontendMessage::Parse(_) => {
                        let (query, is_extended) = match &message {
                            PgWireFrontendMessage::Query(query) => (&query.query, false),
                            PgWireFrontendMessage::Parse(parse) => (&parse.query, true),
                            _ => unreachable!(),
                        };

                        if !is_admin
                            && let Err(err) = cache.query_is_allowed(dashboard_scope.as_ref(), query).await
                        {
                            warn!(?dashboard_scope, ?err, "query rejected by whitelist");
                            send_access_denied(&mut client).await?;

                            if is_extended {
                                rejected_extended_query = true;
                                client.set_state(PgWireConnectionState::AwaitingSync);
                                continue;
                            }

                            client
                                .send(PgWireBackendMessage::ReadyForQuery(ReadyForQuery::new(
                                    transaction_status,
                                )))
                                .await
                                .wrap_err("failed to finish rejected query")?;
                            client.set_state(PgWireConnectionState::ReadyForQuery);
                            continue;
                        }
                    }
                    _ => {}
                }

                let terminate = matches!(&message, PgWireFrontendMessage::Terminate(_));

                server
                    .send(message)
                    .await
                    .wrap_err("failed to forward client message to server")?;

                if terminate {
                    break;
                }
            }

            message = server.next() => {
                let Some(message) = message else {
                    break;
                };
                let message = message.wrap_err("failed to decode server message")?;

                trace!(server = ?message);

                if let PgWireBackendMessage::ReadyForQuery(ready) = &message {
                    transaction_status = ready.status;
                    client.set_state(PgWireConnectionState::ReadyForQuery);
                }

                client
                    .send(message)
                    .await
                    .wrap_err("failed to forward server message to client")?;
            }
        }
    }

    info!("connection closed");
    Ok(())
}

pub struct PgProxy {
    listen_addr: String,
    server_addr: String,
    cache: Arc<WhitelistCache>,
}

impl PgProxy {
    pub fn new(provider: impl QueryTemplateProvider + 'static, config: Config) -> Result<Self> {
        let variable_templates = Arc::new(
            config
                .variable_templates
                .into_iter()
                .map(|(name, template)| {
                    let matcher = VariableTemplateMatcher::parse(&template)
                        .wrap_err_with(|| format!("invalid variable template for {name:?}"))?;
                    Ok((name, matcher))
                })
                .collect::<Result<_>>()?,
        );

        Ok(Self {
            listen_addr: config.proxy.listen_addr,
            server_addr: config.proxy.server_addr,
            cache: Arc::new(WhitelistCache::new(Arc::new(provider), variable_templates)),
        })
    }

    pub async fn run(self) -> Result<()> {
        let listener = TcpListener::bind(&self.listen_addr)
            .await
            .wrap_err_with(|| format!("failed to bind proxy listener to {}", self.listen_addr))?;

        info!(
            listen_addr = %self.listen_addr,
            server_addr = %self.server_addr,
            "proxy listening"
        );

        loop {
            let (client_socket, client_addr) =
                listener.accept().await.wrap_err("failed to accept client connection")?;
            let server_addr = self.server_addr.clone();
            let cache = Arc::clone(&self.cache);

            tokio::spawn(
                async move {
                    if let Err(err) =
                        proxy_connection(client_socket, client_addr, &server_addr, cache).await
                    {
                        error!(?err, "connection failed");
                    }
                }
                .instrument(info_span!("connection", %client_addr)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use parking_lot::RwLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[derive(Default)]
    struct FakeProvider {
        templates: RwLock<HashMap<DashboardScope, Vec<String>>>,
        fetches: AtomicUsize,
        fail: AtomicBool,
        stall: AtomicBool,
        requested_scopes: RwLock<Vec<DashboardScope>>,
    }

    #[async_trait]
    impl QueryTemplateProvider for FakeProvider {
        async fn query_templates(&self, org_id: u64, dashboard_uid: &str) -> Result<Vec<String>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if self.stall.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            tokio::task::yield_now().await;
            ensure!(!self.fail.load(Ordering::SeqCst), "scripted fetch failure");

            let scope = DashboardScope { org_id, dashboard_uid: dashboard_uid.into() };
            self.requested_scopes.write().push(scope.clone());
            self.templates.read().get(&scope).cloned().ok_or_eyre("unexpected scope")
        }
    }

    fn scope(org_id: u64, dashboard_uid: &str) -> DashboardScope {
        DashboardScope { org_id, dashboard_uid: dashboard_uid.into() }
    }

    fn cache_with_templates(
        templates: impl IntoIterator<Item = (DashboardScope, &'static str)>,
    ) -> (WhitelistCache, Arc<FakeProvider>) {
        let provider = Arc::new(FakeProvider {
            templates: RwLock::new(
                templates.into_iter().map(|(scope, sql)| (scope, vec![sql.into()])).collect(),
            ),
            ..Default::default()
        });
        let cache = WhitelistCache::new(provider.clone(), Arc::new(HashMap::new()));
        (cache, provider)
    }

    async fn send_frame(socket: &mut TcpStream, tag: u8, body: &[u8]) {
        socket.write_u8(tag).await.unwrap();
        socket.write_u32((body.len() + 4) as u32).await.unwrap();
        socket.write_all(body).await.unwrap();
    }

    async fn read_frame(socket: &mut TcpStream) -> (u8, Vec<u8>) {
        let tag = socket.read_u8().await.unwrap();
        let length = socket.read_u32().await.unwrap();
        assert!((4..=65536).contains(&length));
        let mut body = vec![0; (length - 4) as usize];
        socket.read_exact(&mut body).await.unwrap();
        (tag, body)
    }

    async fn assert_ready(socket: &mut TcpStream) {
        assert_eq!(read_frame(socket).await, (b'Z', b"I".to_vec()));
    }

    async fn assert_denied(socket: &mut TcpStream) {
        let (tag, body) = read_frame(socket).await;
        assert_eq!(tag, b'E');
        assert!(body.windows(7).any(|field| field == b"C42501\0"));
    }

    async fn protocol_session(
        parameters: &[(&str, &str)], cache: WhitelistCache,
        exercise: impl AsyncFnOnce(TcpStream, TcpStream),
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
            let (socket, address) = listener.accept().await.unwrap();
            let server_address = upstream.local_addr().unwrap().to_string();
            let proxy = proxy_connection(socket, address, &server_address, Arc::new(cache));
            let exchange = async {
                let (mut server, _) = upstream.accept().await.unwrap();
                let mut startup = 196608_u32.to_be_bytes().to_vec();
                for (key, value) in std::iter::once(&("user", "test")).chain(parameters) {
                    startup.extend_from_slice(format!("{key}\0{value}\0").as_bytes());
                }
                startup.push(0);
                client.write_u32((startup.len() + 4) as u32).await.unwrap();
                client.write_all(&startup).await.unwrap();

                let length = server.read_u32().await.unwrap();
                assert_eq!(length as usize, startup.len() + 4);
                let mut forwarded = vec![0; startup.len()];
                server.read_exact(&mut forwarded).await.unwrap();
                assert_eq!(&forwarded[..4], &196608_u32.to_be_bytes());
                send_frame(&mut server, b'R', &0_u32.to_be_bytes()).await;
                send_frame(&mut server, b'Z', b"I").await;
                assert_eq!(read_frame(&mut client).await, (b'R', vec![0; 4]));
                assert_ready(&mut client).await;

                exercise(client, server).await;
            };
            let (result, ()) = tokio::join!(proxy, exchange);
            result.unwrap();
        })
        .await
        .expect("protocol session timed out");
    }

    async fn finish_session(client: &mut TcpStream, server: &mut TcpStream) {
        send_frame(client, b'X', b"").await;
        assert_eq!(read_frame(server).await, (b'X', vec![]));
        assert_eq!(server.read(&mut [0]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn protocol_scope_filters_queries_and_recovers_after_rejected_parse() {
        let (cache, provider) = cache_with_templates([
            (scope(42, "first"), "-- report\nSELECT 1"),
            (scope(42, "second"), "-- report\nSELECT 2"),
            (scope(7, "first"), "-- report\nSELECT 3"),
        ]);

        protocol_session(
            &[("grafana.org_id", "42"), ("grafana.dashboard_uid", "first")],
            cache,
            async |mut client, mut server| {
                let allowed = b"-- report\nSELECT 1\0";
                send_frame(&mut client, b'Q', allowed).await;
                assert_eq!(read_frame(&mut server).await, (b'Q', allowed.to_vec()));
                send_frame(&mut server, b'Z', b"I").await;
                assert_ready(&mut client).await;
                assert_eq!(*provider.requested_scopes.read(), vec![scope(42, "first")]);

                for denied in [b"-- report\nSELECT 2\0", b"-- report\nSELECT 3\0"] {
                    send_frame(&mut client, b'Q', denied).await;
                    assert_denied(&mut client).await;
                    assert_ready(&mut client).await;
                }
                send_frame(&mut client, b'P', b"\0-- report\nSELECT 2\0\0\0").await;
                assert_denied(&mut client).await;
                // Even an allowed query must be discarded until Sync after a denied Parse.
                send_frame(&mut client, b'B', &[0; 8]).await;
                send_frame(&mut client, b'E', &[0; 5]).await;
                send_frame(&mut client, b'H', b"").await;
                send_frame(&mut client, b'Q', allowed).await;
                send_frame(&mut client, b'S', b"").await;
                assert_ready(&mut client).await;

                // The next upstream frame must be this recovery query, not any denied frame.
                send_frame(&mut client, b'Q', allowed).await;
                assert_eq!(read_frame(&mut server).await, (b'Q', allowed.to_vec()));
                send_frame(&mut server, b'Z', b"I").await;
                assert_ready(&mut client).await;
                finish_session(&mut client, &mut server).await;
            },
        )
        .await;

        assert_eq!(provider.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn protocol_missing_scope_denies_queries_but_admin_bypasses() {
        for admin in [false, true] {
            let (cache, provider) = cache_with_templates([]);
            let parameters = if admin { vec![("grafana.role", "Admin")] } else { vec![] };

            protocol_session(&parameters, cache, async |mut client, mut server| {
                let query = b"SELECT 123\0";
                send_frame(&mut client, b'Q', query).await;
                if admin {
                    assert_eq!(read_frame(&mut server).await, (b'Q', query.to_vec()));
                    send_frame(&mut server, b'Z', b"I").await;
                } else {
                    assert_denied(&mut client).await;
                }
                assert_ready(&mut client).await;
                finish_session(&mut client, &mut server).await;
            })
            .await;

            assert_eq!(provider.fetches.load(Ordering::SeqCst), 0);
            assert!(provider.requested_scopes.read().is_empty());
        }
    }

    #[test]
    fn startup_scope_requires_complete_valid_parameters() {
        for (org_id, uid) in [
            (None, None),
            (Some("1"), None),
            (None, Some("dashboard")),
            (Some(""), Some("dashboard")),
            (Some("1"), Some("")),
            (Some(""), Some("")),
        ] {
            let parameters =
                [(GRAFANA_ORG_ID_PARAMETER, org_id), (GRAFANA_DASHBOARD_UID_PARAMETER, uid)]
                    .into_iter()
                    .filter_map(|(key, value)| value.map(|value| (key.into(), value.into())))
                    .collect();

            assert_eq!(DashboardScope::from_parameters(&parameters).unwrap(), None);
        }

        for (org_id, uid) in [
            ("0", "dashboard"),
            ("-1", "dashboard"),
            ("abc", "dashboard"),
            ("18446744073709551616", "dashboard"),
            ("1", "bad/uid"),
            ("1", "bad uid"),
            ("1", " "),
        ] {
            let parameters = BTreeMap::from([
                (GRAFANA_ORG_ID_PARAMETER.into(), org_id.into()),
                (GRAFANA_DASHBOARD_UID_PARAMETER.into(), uid.into()),
            ]);

            assert!(DashboardScope::from_parameters(&parameters).is_err(), "{parameters:?}");
        }

        let parameters = BTreeMap::from([
            (GRAFANA_ORG_ID_PARAMETER.into(), "42".into()),
            (GRAFANA_DASHBOARD_UID_PARAMETER.into(), "Dash_01-ab".into()),
        ]);

        assert_eq!(
            DashboardScope::from_parameters(&parameters).unwrap(),
            Some(scope(42, "Dash_01-ab")),
        );
    }

    #[tokio::test]
    async fn missing_scope_and_exact_ping_never_fetch() {
        let (cache, provider) = cache_with_templates([]);
        let dashboard = scope(1, "dashboard");

        for context in [None, Some(&dashboard)] {
            for query in ["-- ping", " \n-- ping\t\n"] {
                assert!(cache.query_is_allowed(context, query).await.is_ok());
            }
        }
        for query in ["SELECT 1", "-- report\nSELECT 1", "-- ping\nSELECT 1"] {
            assert!(cache.query_is_allowed(None, query).await.is_err());
        }

        assert_eq!(provider.fetches.load(Ordering::SeqCst), 0);
        assert_eq!(cache.dashboards.entry_count(), 0);
    }

    #[tokio::test]
    async fn named_queries_are_isolated_by_dashboard_and_org_and_hits_are_cached() {
        let templates = [
            (scope(1, "first"), "-- report\nSELECT 1"),
            (scope(1, "second"), "-- report\nSELECT 2"),
            (scope(2, "first"), "-- report\nSELECT 3"),
        ];
        let (cache, provider) = cache_with_templates(templates.clone());

        for (dashboard, query) in &templates {
            assert!(cache.query_is_allowed(Some(dashboard), query).await.is_ok());
        }
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 3);

        for (dashboard, _) in &templates {
            for (owner, query) in &templates {
                assert_eq!(
                    cache.query_is_allowed(Some(dashboard), query).await.is_ok(),
                    dashboard == owner,
                );
            }
            assert!(cache.query_is_allowed(Some(dashboard), "-- ping\nSELECT 1").await.is_err());
        }

        assert_eq!(provider.fetches.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn concurrent_same_scope_fetches_once() {
        let dashboard = scope(1, "dashboard");
        let query = "-- report\nSELECT 1";
        let (cache, provider) = cache_with_templates([(dashboard.clone(), query)]);

        let results = futures::future::join_all(
            (0..16).map(|_| cache.query_is_allowed(Some(&dashboard), query)),
        )
        .await;

        assert!(results.iter().all(Result::is_ok));
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_failures_are_cached_until_invalidated() {
        let dashboard = scope(1, "dashboard");
        let query = "-- report\nSELECT 1";
        let (cache, provider) = cache_with_templates([(dashboard.clone(), query)]);
        provider.fail.store(true, Ordering::SeqCst);

        let results = futures::future::join_all(
            (0..16).map(|_| cache.query_is_allowed(Some(&dashboard), query)),
        )
        .await;

        for result in results {
            assert!(result.unwrap_err().to_string().contains("scripted fetch failure"));
        }
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 1);

        provider.fail.store(false, Ordering::SeqCst);
        let error = cache.query_is_allowed(Some(&dashboard), query).await.unwrap_err();
        assert!(error.to_string().contains("scripted fetch failure"));
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 1);

        cache.dashboards.invalidate(&dashboard).await;
        cache.query_is_allowed(Some(&dashboard), query).await.unwrap();
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn whitelist_cache_has_bounded_capacity_and_ttl() {
        let (cache, _) = cache_with_templates([]);
        let policy = cache.dashboards.policy();

        assert_eq!(policy.time_to_live(), Some(Duration::from_secs(30)));
        assert_eq!(policy.max_capacity(), Some(1024));
    }

    #[tokio::test]
    async fn expired_successes_and_failures_are_refetched() {
        for fail in [false, true] {
            let dashboard = scope(1, "dashboard");
            let old_query = "-- report\nSELECT 1";
            let new_query = "-- report\nSELECT 2";
            let (mut cache, provider) = cache_with_templates([(dashboard.clone(), old_query)]);
            cache.dashboards = Cache::builder().time_to_live(Duration::from_millis(20)).build();
            provider.fail.store(fail, Ordering::SeqCst);

            assert_eq!(cache.query_is_allowed(Some(&dashboard), old_query).await.is_err(), fail);
            assert_eq!(provider.fetches.load(Ordering::SeqCst), 1);
            provider.fail.store(false, Ordering::SeqCst);
            provider.templates.write().insert(dashboard.clone(), vec![new_query.into()]);
            // Moka expiration uses the real clock, not Tokio's paused clock.
            tokio::time::sleep(Duration::from_millis(60)).await;

            cache.query_is_allowed(Some(&dashboard), new_query).await.unwrap();
            assert_eq!(provider.fetches.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn capacity_eviction_bounds_successes_and_failures_and_allows_refetch() {
        let query = "-- report\nSELECT 1";
        let dashboards: Vec<_> = (1..=8).map(|org| scope(org, "dashboard")).collect();
        let (mut cache, provider) = cache_with_templates(
            dashboards.iter().step_by(2).cloned().map(|dashboard| (dashboard, query)),
        );
        cache.dashboards = Cache::builder().max_capacity(2).build();

        for (index, dashboard) in dashboards.iter().enumerate() {
            assert_eq!(
                cache.query_is_allowed(Some(dashboard), query).await.is_ok(),
                index % 2 == 0
            );
        }
        cache.dashboards.run_pending_tasks().await;

        assert_eq!(provider.fetches.load(Ordering::SeqCst), dashboards.len());
        assert!(cache.dashboards.entry_count() > 0);
        assert!(cache.dashboards.entry_count() <= 2);
        let evicted =
            dashboards.iter().find(|scope| !cache.dashboards.contains_key(*scope)).unwrap();
        provider.templates.write().insert(evicted.clone(), vec![query.into()]);

        cache.query_is_allowed(Some(evicted), query).await.unwrap();
        assert_eq!(provider.fetches.load(Ordering::SeqCst), dashboards.len() + 1);
        cache.dashboards.run_pending_tasks().await;
        assert!(cache.dashboards.entry_count() <= 2);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_fetch_times_out_once_without_blocking_other_scopes() {
        let dashboard = scope(1, "stalled");
        let other = scope(1, "other");
        let query = "-- report\nSELECT 1";
        let (cache, provider) = cache_with_templates([(other.clone(), query)]);
        provider.stall.store(true, Ordering::SeqCst);
        let started = tokio::time::Instant::now();
        let requests = futures::future::join_all(
            (0..16).map(|_| cache.query_is_allowed(Some(&dashboard), query)),
        );
        tokio::pin!(requests);

        assert!(futures::poll!(&mut requests).is_pending());
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 1);
        provider.stall.store(false, Ordering::SeqCst);
        cache.query_is_allowed(Some(&other), query).await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 2);

        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(futures::poll!(&mut requests).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        for result in requests.await {
            assert!(
                result.unwrap_err().to_string().contains("dashboard whitelist fetch timed out")
            );
        }
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        let error = cache.query_is_allowed(Some(&dashboard), query).await.unwrap_err();
        assert!(error.to_string().contains("dashboard whitelist fetch timed out"));
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalidated_whitelist_is_replaced_with_new_data() {
        let dashboard = scope(1, "dashboard");
        let old_query = "-- report\nSELECT 1";
        let new_query = "-- report\nSELECT 2";
        let (cache, provider) = cache_with_templates([(dashboard.clone(), old_query)]);
        cache.query_is_allowed(Some(&dashboard), old_query).await.unwrap();
        provider.templates.write().insert(dashboard.clone(), vec![new_query.into()]);
        cache.dashboards.invalidate(&dashboard).await;

        assert!(cache.query_is_allowed(Some(&dashboard), new_query).await.is_ok());
        assert!(cache.query_is_allowed(Some(&dashboard), old_query).await.is_err());
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_refresh_denies_stale_queries_and_retries() {
        let dashboard = scope(1, "dashboard");
        let query = "-- report\nSELECT 1";
        let (cache, provider) = cache_with_templates([(dashboard.clone(), query)]);
        cache.query_is_allowed(Some(&dashboard), query).await.unwrap();
        cache.dashboards.invalidate(&dashboard).await;
        provider.fail.store(true, Ordering::SeqCst);

        assert!(cache.query_is_allowed(Some(&dashboard), query).await.is_err());
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 2);

        provider.fail.store(false, Ordering::SeqCst);

        assert!(cache.query_is_allowed(Some(&dashboard), query).await.is_err());
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 2);
        cache.dashboards.invalidate(&dashboard).await;

        assert!(cache.query_is_allowed(Some(&dashboard), query).await.is_ok());
        assert!(cache.query_is_allowed(Some(&dashboard), query).await.is_ok());
        assert_eq!(provider.fetches.load(Ordering::SeqCst), 3);
    }
}
