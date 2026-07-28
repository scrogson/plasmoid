//! Outbound links to peer nodes, carrying particle messages in order.
//!
//! Two requirements from [#14] shape this, and they pull against each other:
//!
//! - **Ordering.** Messages from one particle to another arrive in send order,
//!   across the node boundary. QUIC streams are independent, so a stream per
//!   message would give no ordering at all.
//! - **`send` never blocks.** A particle must not wait on a connection
//!   handshake, a relay round trip, or QUIC flow control.
//!
//! So each peer gets an unbounded queue drained by **one writer task** owning
//! **one unidirectional stream**. Senders only enqueue, which cannot block;
//! the writer connects lazily and writes frames in queue order. That orders the
//! whole node pair — stronger than the guarantee requires — and is how Erlang's
//! distribution carries messages over a single connection per node pair.
//!
//! The queue is unbounded for the same reason mailboxes are: with
//! fire-and-forget delivery, a bound could only drop silently or block, and
//! blocking is exactly what this exists to avoid.
//!
//! [#14]: https://github.com/scrogson/plasmoid/issues/14

use crate::pid::Pid;
use crate::runtime::PLASMOID_ALPN;
use crate::wire;
use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// One particle message on the wire.
///
/// Distinct from [`wire::Command`], which is the request/response protocol used
/// by external clients. This is the fire-and-forget path: there is no reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Envelope {
    Data {
        target: Pid,
        payload: Vec<u8>,
    },
    Tagged {
        target: Pid,
        ref_id: u64,
        payload: Vec<u8>,
    },
}

/// Frames are length-prefixed so many can share one stream.
///
/// Matches the cap on the request/response path. Mailboxes are unbounded, so a
/// peer that could send arbitrarily large frames could exhaust memory on the
/// receiving node with a single message.
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;

pub fn encode_frame(envelope: &Envelope) -> Result<Vec<u8>> {
    let body = wire::serialize(envelope)?;
    anyhow::ensure!(
        body.len() as u64 <= MAX_FRAME_LEN as u64,
        "message is larger than the {MAX_FRAME_LEN} byte frame limit"
    );
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Read one length-prefixed frame. `Ok(None)` means the peer closed the link.
///
/// Lives beside [`encode_frame`] so the two halves of the format cannot drift.
pub async fn read_frame(recv: &mut iroh::endpoint::RecvStream) -> Result<Option<Envelope>> {
    let mut len_buf = [0u8; 4];
    if recv.read_exact(&mut len_buf).await.is_err() {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len_buf);
    anyhow::ensure!(len <= MAX_FRAME_LEN, "peer sent an oversized frame: {len}");

    let mut body = vec![0u8; len as usize];
    recv.read_exact(&mut body).await?;
    Ok(Some(wire::deserialize(&body)?))
}

/// The queue side of a peer link. The writer task owns the stream.
struct PeerLink {
    frames: mpsc::UnboundedSender<Vec<u8>>,
}

/// Outbound message links, one per peer node.
pub struct PeerLinks {
    endpoint: Endpoint,
    /// A std mutex on purpose: only ever held to look up or insert a queue
    /// handle, never across an await, so senders cannot be parked here.
    links: Mutex<HashMap<EndpointId, Arc<PeerLink>>>,
}

impl std::fmt::Debug for PeerLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerLinks").finish_non_exhaustive()
    }
}

impl PeerLinks {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            links: Mutex::new(HashMap::new()),
        }
    }

    /// Queue a message for a particle on another node.
    ///
    /// **Not async, and never blocks** — that is the point. The message is
    /// handed to the peer's writer task and the caller returns immediately,
    /// whether or not a connection exists yet.
    ///
    /// Fire-and-forget per [#14]: the `Err` here covers only encoding, never
    /// delivery, and never reaches the sending particle.
    pub fn send(&self, node: EndpointId, envelope: &Envelope) -> Result<()> {
        let frame = encode_frame(envelope)?;
        let link = self.link_to(node);
        // Fails only if the writer task is gone, which means the message is
        // dropped — indistinguishable from any other delivery failure.
        let _ = link.frames.send(frame);
        Ok(())
    }

    /// Get the queue for a peer, starting its writer task if this is the first
    /// message to it.
    fn link_to(&self, node: EndpointId) -> Arc<PeerLink> {
        let mut links = self.links.lock().unwrap();
        if let Some(link) = links.get(&node) {
            return link.clone();
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let link = Arc::new(PeerLink { frames: tx });
        links.insert(node, link.clone());
        drop(links);

        tokio::spawn(write_to_peer(self.endpoint.clone(), node, rx));
        link
    }
}

/// Drain a peer's queue onto a single ordered stream, reconnecting as needed.
///
/// Runs until the queue is dropped. Connecting here rather than in `send` is
/// what keeps sending particles off the handshake path.
async fn write_to_peer(
    endpoint: Endpoint,
    node: EndpointId,
    mut frames: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let mut stream: Option<iroh::endpoint::SendStream> = None;

    while let Some(frame) = frames.recv().await {
        // Up to two attempts: the first may find a stream that died since the
        // last write, and reconnecting once is enough to tell.
        for attempt in 0..2 {
            if stream.is_none() {
                match open_link(&endpoint, node).await {
                    Ok(s) => stream = Some(s),
                    Err(e) => {
                        tracing::debug!(peer = %node.fmt_short(), error = %e, "Could not reach peer; message dropped");
                        break;
                    }
                }
            }

            let s = stream.as_mut().expect("just opened");
            match s.write_all(&frame).await {
                Ok(()) => break,
                Err(e) => {
                    tracing::debug!(peer = %node.fmt_short(), error = %e, "Peer link broke");
                    stream = None;
                    if attempt == 1 {
                        tracing::debug!(peer = %node.fmt_short(), "Message dropped after reconnect");
                    }
                }
            }
        }
    }

    tracing::debug!(peer = %node.fmt_short(), "Peer writer stopped");
}

async fn open_link(endpoint: &Endpoint, node: EndpointId) -> Result<iroh::endpoint::SendStream> {
    let conn = endpoint
        .connect(node, PLASMOID_ALPN)
        .await
        .context("failed to connect to peer")?;
    let stream = conn.open_uni().await.context("failed to open peer link")?;
    tracing::debug!(peer = %node.fmt_short(), "Opened peer link");
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::PidGenerator;
    use iroh::SecretKey;

    fn a_pid() -> Pid {
        PidGenerator::new(SecretKey::generate().public()).next()
    }

    #[test]
    fn test_frame_carries_its_length() {
        let env = Envelope::Data {
            target: a_pid(),
            payload: b"hello".to_vec(),
        };
        let frame = encode_frame(&env).unwrap();

        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(
            len,
            frame.len() - 4,
            "the prefix must describe exactly the body that follows"
        );
        assert_eq!(wire::deserialize::<Envelope>(&frame[4..]).unwrap(), env);
    }

    #[test]
    fn test_frames_concatenate_without_ambiguity() {
        // Many messages share one stream, so a reader must be able to split them
        // apart from the prefixes alone.
        let a = Envelope::Data {
            target: a_pid(),
            payload: b"first".to_vec(),
        };
        let b = Envelope::Tagged {
            target: a_pid(),
            ref_id: 7,
            payload: b"second".to_vec(),
        };

        let mut buf = encode_frame(&a).unwrap();
        buf.extend_from_slice(&encode_frame(&b).unwrap());

        let len_a = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        let decoded_a: Envelope = wire::deserialize(&buf[4..4 + len_a]).unwrap();
        let rest = &buf[4 + len_a..];
        let len_b = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        let decoded_b: Envelope = wire::deserialize(&rest[4..4 + len_b]).unwrap();

        assert_eq!(decoded_a, a);
        assert_eq!(decoded_b, b);
    }

    #[test]
    fn test_oversized_messages_are_refused_rather_than_sent() {
        let env = Envelope::Data {
            target: a_pid(),
            payload: vec![0u8; MAX_FRAME_LEN as usize + 1],
        };
        assert!(
            encode_frame(&env).is_err(),
            "a frame the receiver would reject must not be queued"
        );
    }
}
