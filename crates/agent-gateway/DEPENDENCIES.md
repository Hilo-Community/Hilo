# Reviewed dependencies

Every new direct dependency is pinned exactly in Cargo.toml; transitive versions are locked
in Cargo.lock. This crate contains no unsafe code.

- `hyper 1.12.0`, `hyper-util 0.1.21`, `http-body-util 0.1.5`: HTTP/1 framing, bounded request
  bodies, streaming responses and CONNECT upgrades. Reuses versions already in the workspace
  lockfile. No hand-written HTTP parser or TLS interception.
- `reqwest 0.12.28`: upstream HTTPS with rustls and bundled roots, streamed request/response
  bodies. Default features, environment proxies, redirects and decompression are disabled.
  DNS is resolved and checked by the Host, then pinned using `resolve_to_addrs` for that
  request, preserving TLS hostname verification and SNI. No retry/fallback to other origins.
- `tokio 1.54.1`: a per-session runtime owns listener, bounded connections and tunnel tasks;
  shutdown drops the runtime and all connections. Reuses the locked workspace version.
- `bytes 1.12.1`, `futures-util 0.3.34`: stream adapters without collecting SSE bodies.
- `url 2.5.8`: origin validation; no user-supplied URL joins.
- `base64 0.22.1`: standard proxy Basic credentials carrying only a random session token.
- `getrandom 0.3.4`, `constant_time_eq 0.4.2`: OS randomness and token comparisons, matching
  versions used by the PolicyEngine. A token is unique per session and is never an API key.
- `zeroize 1.9.1`: clears the provider key's owned string on drop. HTTP/TLS libraries still
  make transient request-header copies in Host memory; those never enter the sandbox.
- `keyring 3.6.3`: stable cross-platform native credential API, with explicit Apple Keychain,
  Windows Credential Manager and Linux Secret Service backends. No mock backend or plaintext
  fallback is used. Missing/locked stores produce explicit errors. Linux requires the user's
  session Secret Service; it does not fall back to file storage or an in-memory-only keyring.

Primary API references: [Hyper server](https://docs.rs/hyper/1.12.0/hyper/server/conn/http1/struct.Builder.html),
[Reqwest client](https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html),
[Keyring backends](https://docs.rs/keyring/3.6.3/keyring/).
