//! OIDC Authorization Code + PKCE (`LOT-017`). For the managed service only: this never touches
//! a local repository, and nothing here auto-syncs anything — it only turns a successful
//! callback into a verified subject the API can bind a `sessions` row to (`tenant_mw.rs`).

use std::sync::Mutex;

use openidconnect::core::{CoreClient, CoreProviderMetadata, CoreResponseType};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse as OidcTokenResponse,
};

/// The concrete type-state `from_provider_metadata` + `set_redirect_uri` produce: the auth
/// endpoint is always present in discovery metadata (`EndpointSet`); the token/userinfo
/// endpoints are optional per the OIDC spec, so discovery only ever yields `EndpointMaybeSet`
/// for them (checked at the `exchange_code` call, which returns `ConfigurationError` if missing).
type DiscoveredClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("OIDC discovery failed: {0}")]
    Discovery(String),
    #[error("OIDC configuration is invalid: {0}")]
    Config(String),
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    #[error("no matching authorization flow for this state")]
    UnknownState,
}

/// One authorization-code flow in flight, keyed by its CSRF state. The real deployment persists
/// this in the `oidc_flows` table (short-lived rows); an in-process map is enough for the flows
/// this process itself started.
#[derive(Default)]
struct FlowStore {
    flows: Mutex<std::collections::HashMap<String, (PkceCodeVerifier, Nonce)>>,
}

impl FlowStore {
    fn insert(&self, state: String, verifier: PkceCodeVerifier, nonce: Nonce) {
        self.flows.lock().unwrap().insert(state, (verifier, nonce));
    }

    fn take(&self, state: &str) -> Option<(PkceCodeVerifier, Nonce)> {
        self.flows.lock().unwrap().remove(state)
    }
}

pub struct OidcClient {
    metadata: CoreProviderMetadata,
    client_id: ClientId,
    client_secret: ClientSecret,
    redirect_url: RedirectUrl,
    http: reqwest::Client,
    flows: FlowStore,
}

impl OidcClient {
    pub async fn discover(
        issuer_url: &str,
        client_id: &str,
        client_secret: &str,
        redirect_url: &str,
    ) -> Result<Self, OidcError> {
        let issuer = IssuerUrl::new(issuer_url.to_string())
            .map_err(|error| OidcError::Config(error.to_string()))?;
        let redirect_url = RedirectUrl::new(redirect_url.to_string())
            .map_err(|error| OidcError::Config(error.to_string()))?;
        let http = reqwest::Client::new();
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|error| OidcError::Discovery(error.to_string()))?;
        Ok(Self {
            metadata,
            client_id: ClientId::new(client_id.to_string()),
            client_secret: ClientSecret::new(client_secret.to_string()),
            redirect_url,
            http,
            flows: FlowStore::default(),
        })
    }

    fn client(&self) -> DiscoveredClient {
        CoreClient::from_provider_metadata(
            self.metadata.clone(),
            self.client_id.clone(),
            Some(self.client_secret.clone()),
        )
        .set_redirect_uri(self.redirect_url.clone())
    }

    /// Starts a login: the caller redirects the browser to the returned URL.
    #[must_use]
    pub fn authorize_url(&self) -> String {
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf_state, nonce) = self
            .client()
            .authorize_url(
                AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("openid".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .set_pkce_challenge(pkce_challenge)
            .url();
        self.flows
            .insert(csrf_state.secret().clone(), pkce_verifier, nonce);
        auth_url.to_string()
    }

    /// Exchanges the callback's `code` for tokens, verifying the nonce the flow started with.
    /// Returns the verified subject (`sub`) claim.
    pub async fn complete_callback(&self, state: &str, code: &str) -> Result<String, OidcError> {
        let (pkce_verifier, nonce) = self.flows.take(state).ok_or(OidcError::UnknownState)?;
        let client = self.client();
        let token_response = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|error| OidcError::TokenExchange(error.to_string()))?
            .set_pkce_verifier(pkce_verifier)
            .request_async(&self.http)
            .await
            .map_err(|error| OidcError::TokenExchange(error.to_string()))?;
        let id_token = token_response
            .id_token()
            .ok_or_else(|| OidcError::TokenExchange("no id_token in response".into()))?;
        let claims = id_token
            .claims(&client.id_token_verifier(), &nonce)
            .map_err(|error| OidcError::TokenExchange(error.to_string()))?;
        Ok(claims.subject().to_string())
    }
}
