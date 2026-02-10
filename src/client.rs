use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointAddr};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::wire::{self, Request, Response};

/// Target for an actor reference - either local (same runtime) or remote.
enum Target {
    /// Local actor on the same endpoint.
    Local,
    /// Remote actor at a known address.
    Remote(EndpointAddr),
}

/// A typed reference to an actor, enabling function calls over QUIC.
///
/// `ActorRef` abstracts over local and remote actors, providing a uniform
/// `call`/`notify` API. Both local and remote calls go through QUIC, ensuring
/// identical behavior regardless of actor location.
pub struct ActorRef {
    endpoint: Endpoint,
    alpn: Vec<u8>,
    target: Target,
    next_id: AtomicU64,
}

impl ActorRef {
    /// Create a reference to a local actor on the given runtime.
    pub fn local(runtime: &crate::ActorRuntime, alpn: &str) -> Self {
        Self {
            endpoint: runtime.endpoint().clone(),
            alpn: alpn.as_bytes().to_vec(),
            target: Target::Local,
            next_id: AtomicU64::new(1),
        }
    }

    /// Create a reference to a remote actor at the given endpoint address.
    pub fn remote(endpoint: Endpoint, alpn: &str, addr: impl Into<EndpointAddr>) -> Self {
        Self {
            endpoint,
            alpn: alpn.as_bytes().to_vec(),
            target: Target::Remote(addr.into()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Get the ALPN protocol identifier for this actor.
    pub fn alpn(&self) -> &[u8] {
        &self.alpn
    }

    /// Call a function on the actor and return the results.
    ///
    /// Opens a QUIC bidirectional stream, sends a serialized `Request`,
    /// and reads back the `Response`. Returns the result values as
    /// wasm-wave encoded strings.
    pub async fn call(&self, function: &str, args: &[&str]) -> Result<Vec<String>> {
        let response = self.send_request(function, args).await?;

        response
            .result
            .map_err(|e| anyhow::anyhow!("actor returned error: {}", e))
    }

    /// Send a notification to the actor (fire-and-forget).
    ///
    /// Sends the request and discards the response.
    pub async fn notify(&self, function: &str, args: &[&str]) -> Result<()> {
        let _ = self.send_request(function, args).await?;
        Ok(())
    }

    /// Internal: send a request and read the response.
    async fn send_request(&self, function: &str, args: &[&str]) -> Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = Request {
            id,
            function: function.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        };

        let request_bytes =
            wire::serialize(&request).context("failed to serialize request")?;

        let addr: EndpointAddr = match &self.target {
            Target::Local => self.endpoint.id().into(),
            Target::Remote(addr) => addr.clone(),
        };

        let conn = self
            .endpoint
            .connect(addr, &self.alpn)
            .await
            .context("failed to connect to actor")?;

        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .context("failed to open bidirectional stream")?;

        send.write_all(&request_bytes).await?;
        send.finish()?;

        let response_bytes = recv
            .read_to_end(1024 * 1024)
            .await
            .context("failed to read response")?;

        let response: Response =
            wire::deserialize(&response_bytes).context("failed to deserialize response")?;

        Ok(response)
    }
}
