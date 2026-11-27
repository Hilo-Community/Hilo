//! A proxy set in the environment must not see local model traffic. Its own test binary: it
//! changes the process environment.
mod common;
use common::FakeServer;
use model_gateway::{
    ollama::OllamaBackend, CancellationToken, CompletionRequest, ModelGateway, ModelRegistry,
    PrivacyPolicy, TokenBudget,
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn proxy_variables_do_not_reroute_the_local_runtime() {
    let proxy = FakeServer::start("{}", Duration::ZERO).await;
    let ollama = FakeServer::start(r#"{"response":"ok"}"#, Duration::ZERO).await;
    for key in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        std::env::set_var(key, format!("http://{}", proxy.address));
    }
    let mut entry = ModelRegistry::builtin().entries()[0].clone();
    entry.endpoint = format!("http://{}", ollama.address);
    let gateway = ModelGateway::new(ModelRegistry::new(vec![entry.clone()]).unwrap())
        .with_backend(entry.kind, Arc::new(OllamaBackend::default()));
    let completion = gateway
        .complete(
            &entry.id,
            &PrivacyPolicy::strict_local(),
            &TokenBudget::new(1_000_000),
            &CompletionRequest {
                prefix: "x".into(),
                suffix: String::new(),
                max_output_tokens: 4,
                stop: vec![],
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(completion.text, "ok");
    assert_eq!(proxy.connections(), 0);
    assert_eq!(ollama.connections(), 1);
}
