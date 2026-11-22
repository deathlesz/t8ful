//! One provider façade over both catalogs.
//!
//! t8ful talks to two OpenAI-compatible providers at once: OpenRouter and
//! OpenCode Go. Both clients live in the session, the merged catalog lists every
//! model from either, and the model id a turn uses decides where its request
//! goes. Routing lives here so the agent loop only ever sees one `Provider`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use t8ful_core::error::ProviderError;
use t8ful_core::provider::{ModelInfo, Provider, ProviderRequest, ProviderStream};
use t8ful_opencode::OpenCodeGoClient;
use t8ful_openrouter::OpenRouterClient;

/// The provider id OpenRouter models are tagged with.
pub const OPENROUTER: &str = "openrouter";
/// The provider id OpenCode Go models are tagged with.
pub const OPENCODE_GO: &str = t8ful_opencode::PROVIDER_ID;

/// Which client a request is sent to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Route {
    #[default]
    OpenRouter,
    OpencodeGo,
}

impl Route {
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenRouter => OPENROUTER,
            Self::OpencodeGo => OPENCODE_GO,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            OPENROUTER => Some(Self::OpenRouter),
            OPENCODE_GO | "opencode" | "opencode_go" => Some(Self::OpencodeGo),
            _ => None,
        }
    }
}

/// The merged catalog: every model from both providers, plus one warning per
/// provider whose catalog could not be loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub models: Vec<ModelInfo>,
    pub warnings: Vec<String>,
}

/// One provider's live client.
struct Clients {
    openrouter: Option<Arc<dyn Provider>>,
    opencode: Option<Arc<dyn Provider>>,
    openrouter_authenticated: bool,
    opencode_authenticated: bool,
}

impl Clients {
    fn get(&self, route: Route) -> Option<Arc<dyn Provider>> {
        match route {
            Route::OpenRouter => self.openrouter.clone(),
            Route::OpencodeGo => self.opencode.clone(),
        }
    }

    fn authenticated(&self, route: Route) -> bool {
        match route {
            Route::OpenRouter => self.openrouter_authenticated,
            Route::OpencodeGo => self.opencode_authenticated,
        }
    }
}

/// A `Provider` that routes each request to the client that owns its model.
///
/// The catalog is fetched from both providers under one deadline. A provider
/// whose catalog fails is reported as a warning and simply contributes no
/// models; the other one keeps working. Duplicate ids (the same upstream model
/// served by both) stay as two rows so a Go subscriber can pick the Go copy, and
/// the route index remembers both, OpenRouter first.
pub struct RoutingProvider {
    clients: Mutex<Clients>,
    /// Routes per model id in catalog order, rebuilt by every catalog fetch.
    routes: Mutex<BTreeMap<String, Vec<Route>>>,
    /// Provider a model id that appears in no catalog is sent to.
    default_route: Route,
    openrouter_base_url: String,
    opencode_base_url: String,
}

impl std::fmt::Debug for RoutingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingProvider")
            .field("default_route", &self.default_route)
            .finish_non_exhaustive()
    }
}

impl RoutingProvider {
    /// Build both clients from the credentials that are present.
    ///
    /// A provider without a credential contributes no client, so it is skipped
    /// when the catalog is fetched and reported if a request is routed to it.
    /// `/login` adds its client back through [`Self::set_credentials`].
    pub fn new(
        openrouter_api_key: Option<String>,
        openrouter_base_url: impl Into<String>,
        opencode_api_key: Option<String>,
        opencode_base_url: impl Into<String>,
        default_route: Route,
    ) -> Result<Self, ProviderError> {
        let openrouter_base_url = openrouter_base_url.into();
        let opencode_base_url = opencode_base_url.into();
        let clients = build_clients(
            openrouter_api_key.as_deref(),
            &openrouter_base_url,
            opencode_api_key.as_deref(),
            &opencode_base_url,
        )?;

        Ok(Self {
            clients: Mutex::new(clients),
            routes: Mutex::new(BTreeMap::new()),
            default_route,
            openrouter_base_url,
            opencode_base_url,
        })
    }

    /// Wrap already-built clients. Tests use this to route between fakes.
    #[cfg(test)]
    pub fn from_clients(
        openrouter: Option<Arc<dyn Provider>>,
        opencode: Option<Arc<dyn Provider>>,
        default_route: Route,
    ) -> Self {
        Self {
            clients: Mutex::new(Clients {
                openrouter_authenticated: openrouter.is_some(),
                opencode_authenticated: opencode.is_some(),
                openrouter,
                opencode,
            }),
            routes: Mutex::new(BTreeMap::new()),
            default_route,
            openrouter_base_url: crate::config::DEFAULT_BASE_URL.to_owned(),
            opencode_base_url: t8ful_opencode::DEFAULT_BASE_URL.to_owned(),
        }
    }

    /// Whether at least one provider has a credential.
    pub fn has_credentials(&self) -> bool {
        let clients = self
            .clients
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        clients.authenticated(Route::OpenRouter) || clients.authenticated(Route::OpencodeGo)
    }

    /// Whether one provider has a credential.
    pub fn provider_authenticated(&self, route: Route) -> bool {
        self.clients
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .authenticated(route)
    }

    /// Rebuild both clients from the current credentials.
    ///
    /// The route index is cleared because the new credentials may see a
    /// different catalog; the next fetch rebuilds it.
    pub fn set_credentials(
        &self,
        openrouter_api_key: Option<String>,
        opencode_api_key: Option<String>,
    ) -> Result<(), ProviderError> {
        let clients = build_clients(
            openrouter_api_key.as_deref(),
            &self.openrouter_base_url,
            opencode_api_key.as_deref(),
            &self.opencode_base_url,
        )?;

        *self
            .clients
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = clients;
        self.routes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();

        Ok(())
    }

    /// Fetch both catalogs concurrently under one deadline.
    pub async fn catalog(&self, timeout: Duration) -> Catalog {
        let (openrouter, opencode) = {
            let clients = self
                .clients
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());

            (
                clients.get(Route::OpenRouter),
                clients.get(Route::OpencodeGo),
            )
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let both = tokio::join!(
            fetch(openrouter, deadline, OPENROUTER),
            fetch(opencode, deadline, OPENCODE_GO)
        );

        let mut models = Vec::new();
        let mut warnings = Vec::new();
        let mut routes: BTreeMap<String, Vec<Route>> = BTreeMap::new();

        // OpenRouter first, so its copy of a duplicated id is the default one.
        for (result, route) in [(both.0, Route::OpenRouter), (both.1, Route::OpencodeGo)] {
            match result {
                Ok(found) => {
                    for model in found {
                        routes.entry(model.id.clone()).or_default().push(route);
                        models.push(model);
                    }
                }
                Err(warning) => warnings.push(warning),
            }
        }

        *self
            .routes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = routes;

        Catalog { models, warnings }
    }

    /// The route a model id resolved to in the last catalog fetch.
    pub fn route_for(&self, model: &str) -> Option<Route> {
        self.routes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(model.trim())
            .and_then(|routes| routes.first().copied())
    }

    /// Send one request to an explicitly chosen route, falling back to the
    /// catalog index and then to the default provider.
    pub fn stream_with(&self, route: Option<Route>, request: ProviderRequest) -> ProviderStream {
        let (openrouter, opencode) = {
            let clients = self
                .clients
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());

            (
                clients.get(Route::OpenRouter),
                clients.get(Route::OpencodeGo),
            )
        };
        let chosen = route
            .or_else(|| self.route_for(&request.model))
            .unwrap_or(self.default_route);
        let provider = match chosen {
            Route::OpenRouter => openrouter,
            Route::OpencodeGo => opencode,
        };

        match provider {
            Some(provider) => provider.stream(request),
            // The chosen provider has no key: say so instead of silently sending
            // the model somewhere that cannot serve it. With no provider
            // configured at all, the neutral missing-credentials error is what
            // every frontend already recognizes.
            None if !self.has_credentials() => Box::pin(futures_util::stream::once(async move {
                Err(ProviderError::MissingCredentials)
            })),
            None => Box::pin(futures_util::stream::once(async move {
                Err(ProviderError::Auth(format!(
                    "no {} credential is configured; run /login {} and try again",
                    chosen.id(),
                    chosen.id()
                )))
            })),
        }
    }
}

/// The route the session picked, shared with the selector by reference.
///
/// `Provider::stream` takes no route, so the choice travels out of band: the
/// session updates this cell when the user picks a model, and the selector
/// reads it at request time. A request therefore always follows the selection
/// that was in force when it started.
pub type RouteCell = Arc<Mutex<Option<Route>>>;

/// A `Provider` that forwards to the routing façade with the selected route.
///
/// This is what the agent actually holds: one handle for the whole session,
/// stable across credential changes, so no request can reach a stale client.
pub struct RouteSelector {
    routing: Arc<RoutingProvider>,
    route: RouteCell,
}

impl std::fmt::Debug for RouteSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteSelector")
            .field(
                "route",
                &*self
                    .route
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()),
            )
            .finish_non_exhaustive()
    }
}

impl RouteSelector {
    pub fn new(routing: Arc<RoutingProvider>, route: RouteCell) -> Self {
        Self { routing, route }
    }
}

#[async_trait]
impl Provider for RouteSelector {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.routing.list_models().await
    }

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        let route = *self
            .route
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        self.routing.stream_with(route, request)
    }
}

#[async_trait]
impl Provider for RoutingProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let catalog = self.catalog(Duration::from_secs(30)).await;

        if catalog.models.is_empty()
            && let Some(warning) = catalog.warnings.first()
        {
            return Err(ProviderError::Other(warning.clone()));
        }

        Ok(catalog.models)
    }

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        self.stream_with(None, request)
    }
}

/// Build a client per configured credential.
fn build_clients(
    openrouter_api_key: Option<&str>,
    openrouter_base_url: &str,
    opencode_api_key: Option<&str>,
    opencode_base_url: &str,
) -> Result<Clients, ProviderError> {
    let openrouter_authenticated = present(openrouter_api_key);
    let opencode_authenticated = present(opencode_api_key);
    let openrouter: Option<Arc<dyn Provider>> = if openrouter_authenticated {
        Some(Arc::new(OpenRouterClient::with_base_url(
            openrouter_base_url,
            openrouter_api_key.expect("checked above"),
        )?))
    } else {
        None
    };
    let opencode: Option<Arc<dyn Provider>> = if opencode_authenticated {
        Some(Arc::new(OpenCodeGoClient::with_base_url(
            opencode_base_url,
            opencode_api_key.expect("checked above"),
        )?))
    } else {
        None
    };

    Ok(Clients {
        openrouter,
        opencode,
        openrouter_authenticated,
        opencode_authenticated,
    })
}

/// Whether a credential is usable.
fn present(key: Option<&str>) -> bool {
    key.is_some_and(|key| !key.trim().is_empty())
}

/// Fetch one provider's catalog, tagging each model and turning a failure into
/// a warning so the other provider's models still arrive.
async fn fetch(
    provider: Option<Arc<dyn Provider>>,
    deadline: tokio::time::Instant,
    id: &str,
) -> Result<Vec<ModelInfo>, String> {
    let Some(provider) = provider else {
        return Ok(Vec::new());
    };

    let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());

    match tokio::time::timeout(timeout, provider.list_models()).await {
        Ok(Ok(models)) => Ok(models
            .into_iter()
            .map(|mut model| {
                model.provider = Some(id.to_owned());
                model
            })
            .collect()),
        Ok(Err(error)) => Err(format!("could not load the {id} model catalog: {error}")),
        Err(_) => Err(format!(
            "the {id} model catalog request timed out after {} ms",
            timeout.as_millis()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use t8ful_core::error::ProviderError;
    use t8ful_core::provider::{ModelInfo, Provider, ProviderRequest, ProviderStream};

    use super::{Catalog, Route, RoutingProvider};

    /// A provider that serves a fixed catalog and reports which model a stream
    /// was asked for.
    struct FakeProvider {
        models: Vec<String>,
        fail: bool,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            if self.fail {
                return Err(ProviderError::Other("catalog is down".to_owned()));
            }

            Ok(self
                .models
                .iter()
                .map(|id| ModelInfo {
                    id: id.clone(),
                    provider: None,
                    name: None,
                    context_length: None,
                    max_output_tokens: None,
                    supports_images: None,
                    supports_reasoning: None,
                    reasoning: None,
                    prompt_price: None,
                    completion_price: None,
                })
                .collect())
        }

        fn stream(&self, request: ProviderRequest) -> ProviderStream {
            let model = request.model;

            Box::pin(futures_util::stream::once(async move {
                Err(ProviderError::Other(model))
            }))
        }
    }

    fn provider(models: &[&str]) -> Arc<dyn Provider> {
        Arc::new(FakeProvider {
            models: models.iter().map(|id| (*id).to_owned()).collect(),
            fail: false,
        })
    }

    /// The fallback route a routing façade was built with.
    fn provider_default(provider: &RoutingProvider) -> Route {
        provider.default_route
    }

    async fn catalog(provider: &RoutingProvider) -> Catalog {
        provider.catalog(Duration::from_secs(1)).await
    }

    #[tokio::test]
    async fn each_model_is_tagged_with_the_provider_it_came_from() {
        let routing = RoutingProvider::from_clients(
            Some(provider(&["vendor/shared"])),
            Some(provider(&["kimi-k3"])),
            Route::OpenRouter,
        );
        let catalog = catalog(&routing).await;

        assert!(catalog.warnings.is_empty(), "{catalog:?}");
        assert_eq!(
            catalog
                .models
                .iter()
                .map(|model| (model.id.as_str(), model.provider.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("vendor/shared", Some("openrouter")),
                ("kimi-k3", Some("opencode-go")),
            ]
        );
    }

    #[tokio::test]
    async fn a_duplicated_id_stays_two_rows_and_routes_to_the_first() {
        let routing = RoutingProvider::from_clients(
            Some(provider(&["deepseek-v4.1-flash"])),
            Some(provider(&["deepseek-v4.1-flash"])),
            Route::OpencodeGo,
        );
        let catalog = catalog(&routing).await;

        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].provider.as_deref(), Some("openrouter"));
        assert_eq!(catalog.models[1].provider.as_deref(), Some("opencode-go"));
        // The explicit choice wins over the index, which keeps both.
        assert_eq!(
            routing.route_for("deepseek-v4.1-flash"),
            Some(Route::OpenRouter)
        );
    }

    #[tokio::test]
    async fn an_unknown_id_falls_back_to_the_default_provider() {
        let routing = RoutingProvider::from_clients(
            Some(provider(&["vendor/shared"])),
            Some(provider(&["kimi-k3"])),
            Route::OpencodeGo,
        );

        catalog(&routing).await;

        assert_eq!(routing.route_for("hand/typed"), None);
        assert_eq!(provider_default(&routing), Route::OpencodeGo);
    }

    #[tokio::test]
    async fn one_failed_catalog_warns_and_the_other_still_loads() {
        let routing = RoutingProvider::from_clients(
            Some(provider(&["vendor/shared"])),
            Some(Arc::new(FakeProvider {
                models: Vec::new(),
                fail: true,
            })),
            Route::OpenRouter,
        );
        let catalog = catalog(&routing).await;

        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "vendor/shared");
        assert_eq!(catalog.warnings.len(), 1);
        assert!(
            catalog.warnings[0].contains("opencode-go"),
            "{:?}",
            catalog.warnings
        );
    }

    #[tokio::test]
    async fn a_single_key_lists_only_that_catalog() {
        let routing =
            RoutingProvider::from_clients(None, Some(provider(&["kimi-k3"])), Route::OpenRouter);
        let catalog = catalog(&routing).await;

        assert!(catalog.warnings.is_empty(), "{catalog:?}");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].provider.as_deref(), Some("opencode-go"));
        // The default provider has no key, but the only loaded model does, and
        // the index still routes it there.
        assert_eq!(routing.route_for("kimi-k3"), Some(Route::OpencodeGo));
    }

    #[test]
    fn route_ids_round_trip_and_accept_the_short_spelling() {
        assert_eq!(Route::from_id("openrouter"), Some(Route::OpenRouter));
        assert_eq!(Route::from_id("opencode-go"), Some(Route::OpencodeGo));
        assert_eq!(Route::from_id("opencode"), Some(Route::OpencodeGo));
        assert_eq!(Route::from_id("google"), None);
        assert_eq!(Route::OpenRouter.id(), "openrouter");
        assert_eq!(Route::OpencodeGo.id(), "opencode-go");
    }
}
