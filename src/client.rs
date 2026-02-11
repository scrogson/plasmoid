use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointAddr};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::runtime::PLASMOID_ALPN;
use crate::wire::{
    self, CallRequest, CallResponse, Command, CommandResponse, SpawnRequest, SpawnResult, Target,
};

/// A typed reference to an actor, enabling function calls over QUIC.
///
/// `ActorRef` targets actors by name (or PID). All connections use the
/// single `PLASMOID_ALPN` protocol, with target addressing in the request.
pub struct ActorRef {
    endpoint: Endpoint,
    target: ActorTarget,
    next_id: AtomicU64,
}

enum ActorTarget {
    /// Remote actor at a known address.
    Remote {
        addr: EndpointAddr,
        name: String,
    },
}

impl ActorRef {
    /// Create a reference to a remote actor by name at the given endpoint address.
    pub fn remote_by_name(
        endpoint: Endpoint,
        name: &str,
        addr: impl Into<EndpointAddr>,
    ) -> Self {
        Self {
            endpoint,
            target: ActorTarget::Remote {
                addr: addr.into(),
                name: name.to_string(),
            },
            next_id: AtomicU64::new(1),
        }
    }

    /// Call a function on the actor and return the results.
    pub async fn call(&self, function: &str, args: &[&str]) -> Result<Vec<String>> {
        let response = self.send_request(function, args).await?;

        response
            .result
            .map_err(|e| anyhow::anyhow!("actor returned error: {}", e))
    }

    /// Send a notification to the actor (fire-and-forget).
    pub async fn notify(&self, function: &str, args: &[&str]) -> Result<()> {
        let _ = self.send_request(function, args).await?;
        Ok(())
    }

    /// Internal: send a request and read the response.
    async fn send_request(&self, function: &str, args: &[&str]) -> Result<CallResponse> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let (addr, wire_target) = match &self.target {
            ActorTarget::Remote { addr, name } => {
                (addr.clone(), Target::Name(name.clone()))
            }
        };

        let request = CallRequest {
            id,
            target: wire_target,
            function: function.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        };

        let command = Command::Call(request);
        let request_bytes =
            wire::serialize(&command).context("failed to serialize command")?;

        let conn = self
            .endpoint
            .connect(addr, PLASMOID_ALPN)
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

        let response: CommandResponse =
            wire::deserialize(&response_bytes).context("failed to deserialize response")?;

        match response {
            CommandResponse::Call(call_response) => Ok(call_response),
            other => anyhow::bail!("unexpected response type: expected Call, got {:?}", other),
        }
    }
}

/// A client for node-level operations (spawn, not targeted at a specific process).
pub struct NodeClient {
    endpoint: Endpoint,
    addr: EndpointAddr,
}

impl NodeClient {
    pub fn new(endpoint: Endpoint, addr: impl Into<EndpointAddr>) -> Self {
        Self {
            endpoint,
            addr: addr.into(),
        }
    }

    /// Spawn a process on the remote node from a registered component.
    pub async fn spawn(
        &self,
        component: &str,
        name: Option<&str>,
    ) -> Result<SpawnResult> {
        let command = Command::Spawn(SpawnRequest {
            component: component.to_string(),
            name: name.map(|s| s.to_string()),
        });

        let response = self.send_command(&command).await?;

        match response {
            CommandResponse::Spawn(spawn_response) => spawn_response
                .result
                .map_err(|e| anyhow::anyhow!("spawn failed: {}", e)),
            other => anyhow::bail!("unexpected response type: expected Spawn, got {:?}", other),
        }
    }

    async fn send_command(&self, command: &Command) -> Result<CommandResponse> {
        let request_bytes =
            wire::serialize(command).context("failed to serialize command")?;

        let conn = self
            .endpoint
            .connect(self.addr.clone(), PLASMOID_ALPN)
            .await
            .context("failed to connect to node")?;

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

        let response: CommandResponse =
            wire::deserialize(&response_bytes).context("failed to deserialize response")?;

        Ok(response)
    }
}
