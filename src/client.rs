use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointAddr};

use crate::runtime::PLASMOID_ALPN;
use crate::wire::{
    self, Command, CommandResponse, SendRequest, SpawnRequest, SpawnResult, Target,
};

/// A client for node-level operations (spawn, send).
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

    /// Spawn a particle on the remote node from a registered component.
    pub async fn spawn(
        &self,
        component: &str,
        name: Option<&str>,
        init_msg: &[u8],
    ) -> Result<SpawnResult> {
        let command = Command::Spawn(SpawnRequest {
            component: component.to_string(),
            name: name.map(|s| s.to_string()),
            init_msg: init_msg.to_vec(),
        });

        let response = self.send_command(&command).await?;

        match response {
            CommandResponse::Spawn(spawn_response) => spawn_response
                .result
                .map_err(|e| anyhow::anyhow!("spawn failed: {}", e)),
            other => anyhow::bail!(
                "unexpected response type: expected Spawn, got {:?}",
                other
            ),
        }
    }

    /// Send a message to a particle by name.
    pub async fn send(&self, target: &str, msg: &[u8]) -> Result<()> {
        let command = Command::Send(SendRequest {
            target: Target::Name(target.to_string()),
            msg: msg.to_vec(),
        });

        let response = self.send_command(&command).await?;

        match response {
            CommandResponse::Send(send_response) => send_response
                .result
                .map_err(|e| anyhow::anyhow!("send failed: {}", e)),
            other => anyhow::bail!(
                "unexpected response type: expected Send, got {:?}",
                other
            ),
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
