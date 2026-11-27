//! LOT-014 blocking scenarios for the model gateway.
mod common;
use common::{closed_port, FakeServer};
use host_core::{AuthorizedDestination, ModelProviderKind};
use model_gateway::{
    ollama::OllamaBackend, BackendOutput, BoxFuture, CancellationToken, CompletionRequest,
    GatewayError, ModelBackend, ModelEntry, ModelGateway, ModelRegistry, PrivacyPolicy,
    TokenBudget,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const GENERATED: &str = r#"{"response":"a + b","prompt_eval_count":12,"eval_count":3}"#;

/// A cloud client: it really opens a TCP connection to its endpoint, so a routing mistake
/// would show up as a connection on the server standing in for that provider.
struct CloudBackend;
impl ModelBackend for CloudBackend {
    fn complete<'a>(
        &'a self,
        entry: &'a ModelEntry,
        _request: &'a CompletionRequest,
    ) -> BoxFuture<'a, Result<BackendOutput, String>> {
        Box::pin(async move {
            let url = url::Url::parse(&entry.endpoint).unwrap();
            let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
            tokio::net::TcpStream::connect(address)
                .await
                .map_err(|e| e.to_string())?;
            Err("cloud fixture never answers".into())
        })
    }
}

fn local(endpoint: String) -> ModelEntry {
    let mut entry = ModelRegistry::builtin().entries()[0].clone();
    entry.endpoint = endpoint;
    entry
}

fn cloud(id: &str, destination: AuthorizedDestination, port: u16) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        display_name: id.into(),
        kind: ModelProviderKind::ByokCloud,
        model_name: id.into(),
        destination,
        endpoint: format!("https://127.0.0.1:{port}"),
        min_free_memory_bytes: 0,
        capabilities: local(String::new()).capabilities,
    }
}

fn request() -> CompletionRequest {
    CompletionRequest {
        prefix: "fn add(a: i32, b: i32) -> i32 {\n    ".into(),
        suffix: "\n}\n".into(),
        max_output_tokens: 32,
        stop: vec!["\n\n".into()],
    }
}

fn gateway(entries: Vec<ModelEntry>) -> ModelGateway {
    ModelGateway::new(ModelRegistry::new(entries).unwrap())
        .with_backend(
            ModelProviderKind::LocalOllama,
            Arc::new(OllamaBackend::default()),
        )
        .with_backend(ModelProviderKind::ByokCloud, Arc::new(CloudBackend))
}

/// Section 5 blocking scenario: the configured provider fails while another one exists.
#[tokio::test]
async fn a_failing_provider_never_falls_back_to_another_destination() {
    let other_cloud = FakeServer::start(GENERATED, Duration::ZERO).await;
    let byok = "byok/configured";
    let managed = "cloud/other";
    let gateway = gateway(vec![
        local(format!("http://{}", closed_port())),
        cloud(
            byok,
            AuthorizedDestination::ByokProvider {
                provider_id: "configured".into(),
            },
            closed_port().port(),
        ),
        cloud(
            managed,
            AuthorizedDestination::ManagedCloudEu,
            other_cloud.address.port(),
        ),
    ]);
    let budget = TokenBudget::new(1_000_000);
    let never = CancellationToken::new();

    // Local outage under strict local mode.
    let result = gateway
        .complete(
            "ollama/qwen2.5-coder:1.5b-base",
            &PrivacyPolicy::strict_local(),
            &budget,
            &request(),
            &never,
        )
        .await;
    assert!(
        matches!(result, Err(GatewayError::ProviderUnavailable { ref model_id, .. }) if model_id == "ollama/qwen2.5-coder:1.5b-base"),
        "{result:?}"
    );

    // Outage of the BYOK provider the workspace approved.
    let approved = PrivacyPolicy::allowing(vec![AuthorizedDestination::ByokProvider {
        provider_id: "configured".into(),
    }]);
    let result = gateway
        .complete(byok, &approved, &budget, &request(), &never)
        .await;
    assert!(matches!(
        result,
        Err(GatewayError::ProviderUnavailable { .. })
    ));

    // Asking the unapproved cloud directly is refused before any connection.
    let result = gateway
        .complete(managed, &approved, &budget, &request(), &never)
        .await;
    assert_eq!(
        result,
        Err(GatewayError::PrivacyDenied(
            AuthorizedDestination::ManagedCloudEu
        ))
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        other_cloud.connections(),
        0,
        "nothing may reach an unapproved destination"
    );
    // State is preserved: the failures only cost what their prompts may have used.
    assert_eq!(budget.used(), 2 * request().input_estimate());
}

#[tokio::test]
async fn the_local_runtime_gets_a_fill_in_the_middle_request_and_its_usage_is_counted() {
    let ollama = FakeServer::start(GENERATED, Duration::ZERO).await;
    let gateway = gateway(vec![local(format!("http://{}", ollama.address))]);
    let budget = TokenBudget::new(1_000_000);
    let completion = gateway
        .complete(
            "ollama/qwen2.5-coder:1.5b-base",
            &PrivacyPolicy::strict_local(),
            &budget,
            &request(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(completion.text, "a + b");
    assert_eq!(
        (
            completion.usage.input_tokens,
            completion.usage.output_tokens
        ),
        (12, 3)
    );
    assert_eq!(budget.used(), 15);
    let sent = ollama.requests.lock().unwrap()[0].clone();
    assert!(sent.starts_with("POST /api/generate "), "{sent}");
    let body: serde_json::Value =
        serde_json::from_str(&sent[sent.find("\r\n\r\n").unwrap() + 4..]).unwrap();
    assert_eq!(body["model"], "qwen2.5-coder:1.5b-base");
    assert_eq!(body["suffix"], "\n}\n");
    assert_eq!(body["stream"], false);
    assert_eq!(body["options"]["num_predict"], 32);
}

#[tokio::test]
async fn a_new_keystroke_cancels_the_call_at_once() {
    let slow = FakeServer::start(GENERATED, Duration::from_secs(10)).await;
    let gateway = Arc::new(gateway(vec![local(format!("http://{}", slow.address))]));
    let budget = Arc::new(TokenBudget::new(1_000_000));
    let cancel = CancellationToken::new();
    let call = {
        let (gateway, budget, cancel) = (gateway.clone(), budget.clone(), cancel.clone());
        tokio::spawn(async move {
            gateway
                .complete(
                    "ollama/qwen2.5-coder:1.5b-base",
                    &PrivacyPolicy::strict_local(),
                    &budget,
                    &request(),
                    &cancel,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    cancel.cancel();
    let signalled = started.elapsed();
    let result = call.await.unwrap();
    let finished = started.elapsed();
    assert_eq!(result, Err(GatewayError::Cancelled));
    assert!(
        signalled < Duration::from_millis(1),
        "cancel() took {signalled:?}"
    );
    assert!(
        finished < Duration::from_millis(50),
        "the call ended after {finished:?}"
    );
    assert_eq!(budget.used(), request().input_estimate());
}

#[tokio::test]
async fn the_budget_is_reserved_before_the_runtime_is_contacted() {
    let ollama = FakeServer::start(GENERATED, Duration::ZERO).await;
    let gateway = gateway(vec![local(format!("http://{}", ollama.address))]);
    let budget = TokenBudget::new(request().token_estimate() - 1);
    let result = gateway
        .complete(
            "ollama/qwen2.5-coder:1.5b-base",
            &PrivacyPolicy::strict_local(),
            &budget,
            &request(),
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(GatewayError::BudgetExhausted { .. })));
    assert_eq!(ollama.connections(), 0);
}

/// Real runtime benchmark (LOT-014 blocking): needs `ollama pull qwen2.5-coder:1.5b-base`.
/// Run with `cargo test -p model-gateway --test lot014_gateway -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs a local Ollama with qwen2.5-coder:1.5b-base"]
async fn local_completion_p95_is_at_most_700_ms() {
    let gateway = gateway(ModelRegistry::builtin().entries().to_vec());
    let budget = TokenBudget::new(u64::MAX);
    let policy = PrivacyPolicy::strict_local();
    let model = "ollama/qwen2.5-coder:1.5b-base";
    // The first call loads the model: it is not a keystroke latency.
    gateway
        .complete(
            model,
            &policy,
            &budget,
            &request(),
            &CancellationToken::new(),
        )
        .await
        .expect("Ollama must be running with the model pulled");
    let mut samples = Vec::new();
    for i in 0..40 {
        let mut asked = request();
        asked.prefix = format!("// variant {i}\n{}", asked.prefix);
        let started = Instant::now();
        gateway
            .complete(model, &policy, &budget, &asked, &CancellationToken::new())
            .await
            .unwrap();
        samples.push(started.elapsed());
    }
    samples.sort();
    let p95 = samples[samples.len() * 95 / 100 - 1];
    println!(
        "local completion p50 {:?}, p95 {p95:?}",
        samples[samples.len() / 2]
    );
    assert!(p95 <= Duration::from_millis(700), "p95 {p95:?}");
}
