use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use eyre::{OptionExt, Result, WrapErr, eyre};
use futures::{SinkExt, StreamExt};
use parking_lot::RwLock;
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
    time,
};
use tokio_util::codec::Framed;
use tracing::{Instrument, error, info, info_span, trace, warn};

use crate::{
    config::Config,
    provider::QueryTemplateProvider,
    template::matcher::{query::QueryTemplateMatcher, variable::VariableTemplateMatcher},
};

const WHITELIST_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

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

type Whitelist = Arc<RwLock<QueryWhitelist>>;

pub struct PgProxy {
    listen_addr: String,
    server_addr: String,
    provider: Arc<dyn QueryTemplateProvider>,
    variable_templates: Arc<HashMap<String, VariableTemplateMatcher>>,
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
            provider: Arc::new(provider),
            variable_templates,
        })
    }

    pub async fn run(self) -> Result<()> {
        let initial_whitelist = fetch_whitelist(&*self.provider, &self.variable_templates)
            .await
            .wrap_err("failed to initialize query whitelist")?;
        let whitelist = Arc::new(RwLock::new(initial_whitelist));
        let query_count = whitelist.read().len();

        info!(queries = query_count, "query whitelist initialized");

        tokio::spawn(refresh_whitelist(
            Arc::clone(&self.provider),
            self.variable_templates,
            Arc::clone(&whitelist),
        ));

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
            let whitelist = Arc::clone(&whitelist);

            tokio::spawn(
                async move {
                    if let Err(err) =
                        proxy_connection(client_socket, client_addr, &server_addr, whitelist).await
                    {
                        error!(?err, "connection failed");
                    }
                }
                .instrument(info_span!("connection", %client_addr)),
            );
        }
    }
}

async fn refresh_whitelist(
    provider: Arc<dyn QueryTemplateProvider>,
    variable_templates: Arc<HashMap<String, VariableTemplateMatcher>>, whitelist: Whitelist,
) {
    let mut interval = time::interval(WHITELIST_REFRESH_INTERVAL);
    interval.tick().await;

    loop {
        interval.tick().await;

        match fetch_whitelist(&*provider, &variable_templates).await {
            Ok(matchers) => {
                let query_count = matchers.len();
                *whitelist.write() = matchers;
                info!(queries = query_count, "query whitelist refreshed");
            }
            Err(err) => {
                error!(?err, "failed to refresh query whitelist; keeping previous whitelist");
            }
        }
    }
}

async fn fetch_whitelist(
    provider: &dyn QueryTemplateProvider,
    variable_templates: &Arc<HashMap<String, VariableTemplateMatcher>>,
) -> Result<QueryWhitelist> {
    let query_templates = provider.query_templates().await?;
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

async fn proxy_connection(
    client_socket: TcpStream, client_addr: SocketAddr, server_addr: &str, whitelist: Whitelist,
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
                            && let Err(err) = query_is_allowed(&whitelist, query)
                        {
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

fn query_is_allowed(whitelist: &Whitelist, query: &str) -> Result<()> {
    let whitelist = whitelist.read();

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
