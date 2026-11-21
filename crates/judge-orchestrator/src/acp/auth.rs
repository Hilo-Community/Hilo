//! Generic ACP v1 authentication negotiation. Provider and agent knowledge comes from the Host.
use super::protocol::{bad, AcpError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub enum AgentCredentials {
    #[default]
    Unauthenticated,
    /// Retained as an explicit error for callers that have not supplied a proxy.
    HostMediated { description: String },
    Gateway {
        protocol: String,
        base_url: String,
        headers: BTreeMap<String, String>,
        provider_name: String,
        /// A reviewed profile has already configured the base URL and session-token env.
        environment_fallback: bool,
    },
    Account {
        use_stored_credentials: bool,
        method_id: Option<String>,
        preferred_methods: Vec<String>,
        browser_hosts: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default, rename = "_meta")]
    pub meta: Value,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

pub(crate) enum AuthenticationPlan {
    Skip,
    Request(Value),
    Terminal(AuthMethod),
}

pub(crate) fn methods(value: &Value) -> Result<Vec<AuthMethod>, AcpError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let mut methods: Vec<AuthMethod> =
        serde_json::from_value(value.clone()).map_err(|_| bad("invalid authMethods"))?;
    for method in &mut methods {
        if method.kind.is_none() {
            if let Some(kind) = method.meta.get("type").and_then(Value::as_str) {
                method.kind = Some(kind.to_owned());
            } else if method.meta.get("terminal-auth").is_some() {
                method.kind = Some("terminal".to_owned());
            }
        }
        if method.args.is_empty() {
            let meta_args = method
                .meta
                .get("args")
                .or_else(|| method.meta.get("terminal-auth").and_then(|v| v.get("args")));
            if let Some(args) = meta_args {
                if let Ok(parsed) = serde_json::from_value::<Vec<String>>(args.clone()) {
                    method.args = parsed;
                }
            }
        }
        if method.env.is_empty() {
            let meta_env = method
                .meta
                .get("env")
                .or_else(|| method.meta.get("terminal-auth").and_then(|v| v.get("env")));
            if let Some(env) = meta_env {
                if let Ok(parsed) = serde_json::from_value::<BTreeMap<String, String>>(env.clone())
                {
                    method.env = parsed;
                }
            }
        }
    }
    let mut ids = std::collections::BTreeSet::new();
    if methods.len() > 32
        || methods.iter().any(|m| {
            m.id.is_empty() || m.id.len() > 256 || m.name.len() > 512 || !ids.insert(&m.id)
        })
    {
        return Err(bad("invalid authMethods count, identifier or duplicate"));
    }
    Ok(methods)
}

pub(crate) fn authentication(
    credentials: &AgentCredentials,
    methods: &[AuthMethod],
) -> Result<AuthenticationPlan, AcpError> {
    match credentials {
        AgentCredentials::Unauthenticated => Ok(AuthenticationPlan::Skip),
        AgentCredentials::HostMediated { .. } => Err(bad("authentication requires a Host proxy")),
        AgentCredentials::Gateway {
            protocol,
            base_url,
            headers,
            provider_name,
            environment_fallback,
        } => {
            let method = methods.iter().find(|m| {
                m.meta["gateway"]["protocol"] == *protocol
                    && m.kind.as_deref().unwrap_or("agent") == "agent"
            });
            match method {
                Some(method) => Ok(AuthenticationPlan::Request(json!({"methodId":method.id,"_meta":{"gateway":{"baseUrl":base_url,"headers":headers,"providerName":provider_name}}}))),
                None if *environment_fallback => Ok(AuthenticationPlan::Skip),
                None => Err(bad("agent does not advertise a compatible gateway and has no reviewed environment fallback")),
            }
        }
        AgentCredentials::Account {
            method_id,
            preferred_methods,
            ..
        } => {
            let method = match method_id {
                Some(id) => methods.iter().find(|m| &m.id == id),
                None => preferred_methods
                    .iter()
                    .find_map(|id| methods.iter().find(|m| &m.id == id)),
            }
            .ok_or_else(|| bad("agent does not advertise the selected account login"))?;
            match method.kind.as_deref().unwrap_or("agent") {
                "agent" => Ok(AuthenticationPlan::Request(json!({"methodId":method.id}))),
                "terminal" => Ok(AuthenticationPlan::Terminal(method.clone())),
                _ => Err(bad("unsupported authentication method type")),
            }
        }
    }
}

pub(crate) fn browser_url(credentials: &AgentCredentials, url: &str) -> Result<(), AcpError> {
    let AgentCredentials::Account { browser_hosts, .. } = credentials else {
        return Err(bad("URL authentication is unavailable in this mode"));
    };
    let url = url::Url::parse(url).map_err(|_| bad("invalid authentication URL"))?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || !url
            .host_str()
            .is_some_and(|host| browser_hosts.iter().any(|allowed| host == allowed))
    {
        return Err(bad("authentication URL host is not approved"));
    }
    Ok(())
}

pub(crate) fn terminal_spec(
    base: &sandbox_runner::SandboxSpec,
    method: &AuthMethod,
) -> Result<sandbox_runner::SandboxSpec, AcpError> {
    if method.args.len() > 64
        || method.args.iter().map(String::len).sum::<usize>() > 24 * 1024
        || method.args.iter().any(|arg| arg.contains('\0'))
        || method.env.len() > 32
    {
        return Err(bad("terminal login descriptor exceeds limits"));
    }
    let mut spec = base.clone();
    for (key, value) in &method.env {
        let upper = key.to_ascii_uppercase();
        if base
            .env
            .keys()
            .any(|existing| existing.to_string_lossy().eq_ignore_ascii_case(key))
            || upper.contains("PROXY")
            || upper.contains("CERT")
            || upper.starts_with("LD_")
            || upper.starts_with("DYLD_")
            || [
                "PATH",
                "HOME",
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "ENV",
                "BASH_ENV",
                "ZDOTDIR",
                "SHELL",
                "NODE_OPTIONS",
                "NODE_PATH",
                "PYTHONPATH",
                "PYTHONHOME",
                "PERL5LIB",
                "RUBYOPT",
                "RUBYLIB",
                "GIT_CONFIG",
                "GIT_CONFIG_SYSTEM",
                "GIT_CONFIG_GLOBAL",
            ]
            .contains(&upper.as_str())
            || key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || value.len() > 8192
            || value.contains('\0')
        {
            return Err(bad(
                "terminal login cannot override home, interpreter, proxy or TLS configuration",
            ));
        }
        spec.env.insert(key.into(), value.into());
    }
    spec.args
        .retain(|arg| arg != "--acp" && arg != "--experimental-acp");
    spec.args.extend(method.args.iter().map(Into::into));
    spec.env.insert("TERM".into(), "xterm-256color".into());
    spec.env.insert("TERM_PROGRAM".into(), "Coloride".into());
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gateway_selection_uses_protocol_metadata_never_an_agent_or_method_name() {
        let methods = methods(&json!([
            {"id":"other", "name":"Other", "_meta":{"gateway":{"protocol":"anthropic"}}},
            {"id":"custom-name", "name":"Gateway", "_meta":{"gateway":{"protocol":"openai"}}}
        ]))
        .unwrap();
        let credentials = AgentCredentials::Gateway {
            protocol: "openai".into(),
            base_url: "http://127.0.0.1:123/openai".into(),
            headers: BTreeMap::from([("x-coloride-session".into(), "session-only".into())]),
            provider_name: "Provider".into(),
            environment_fallback: false,
        };
        let AuthenticationPlan::Request(request) = authentication(&credentials, &methods).unwrap()
        else {
            panic!("missing gateway request")
        };
        assert_eq!(request["methodId"], "custom-name");
        assert_eq!(
            request["_meta"]["gateway"]["headers"]["x-coloride-session"],
            "session-only"
        );
        assert!(authentication(&credentials, &[]).is_err());
    }
    #[test]
    fn account_selection_is_explicit_and_unknown_methods_and_urls_fail_closed() {
        let methods = methods(&json!([{ "id":"login", "name":"Login" }, {"id":"interactive", "name":"Terminal", "type":"terminal", "args":["--login"]}])).unwrap();
        let mut credentials = AgentCredentials::Account {
            use_stored_credentials: false,
            method_id: None,
            preferred_methods: vec!["login".into()],
            browser_hosts: vec!["auth.example.com".into()],
        };
        let AuthenticationPlan::Request(request) = authentication(&credentials, &methods).unwrap()
        else {
            panic!("missing login request")
        };
        assert_eq!(request["methodId"], "login");
        assert!(browser_url(&credentials, "https://auth.example.com/device").is_ok());
        for url in [
            "http://auth.example.com",
            "https://auth.example.com.evil.com",
            "https://auth.example.com@evil.com",
            "https://127.0.0.1",
            "https://auth.example.com:8443",
        ] {
            assert!(browser_url(&credentials, url).is_err());
        }
        if let AgentCredentials::Account { method_id, .. } = &mut credentials {
            *method_id = Some("interactive".into());
        }
        assert!(matches!(
            authentication(&credentials, &methods).unwrap(),
            AuthenticationPlan::Terminal(_)
        ));
        let meta_terminal = super::methods(&json!([{
            "id": "openai",
            "name": "Use OpenAI API key",
            "_meta": {
                "type": "terminal",
                "args": ["--auth-type=openai"]
            }
        }]))
        .unwrap();
        assert_eq!(meta_terminal[0].kind.as_deref(), Some("terminal"));
        assert_eq!(meta_terminal[0].args, vec!["--auth-type=openai"]);
        let mut base = sandbox_runner::SandboxSpec::new(".", "/bin/node");
        base.args = vec!["cli-entry.js".into(), "--acp".into()];
        let login_spec = terminal_spec(&base, &meta_terminal[0]).unwrap();
        assert_eq!(
            login_spec.args,
            vec![
                std::ffi::OsString::from("cli-entry.js"),
                std::ffi::OsString::from("--auth-type=openai")
            ]
        );
    }
}
