//! TLS for outbound HTTPS: the LINE API and the AI providers.

use std::sync::Arc;

use anyhow::Context;

/// TLS with ring and the compiled-in roots, like the database connection. No certificate
/// settings are read from the environment (CFG-02).
pub fn client_config() -> anyhow::Result<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("cannot set up TLS")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}
