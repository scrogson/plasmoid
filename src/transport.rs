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

use crate::message::ExitReason;
use crate::pid::Pid;
use crate::runtime::PLASMOID_ALPN;
use crate::wire;
use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Who a message is addressed to, once it has crossed to the target node.
///
/// A name is carried unresolved: it is looked up in the *receiving* node's
/// registry, which is what makes a `named` destination need no round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Addressee {
    Pid(Pid),
    Name(String),
}

/// One particle message on the wire.
///
/// Distinct from [`wire::Command`], which is the request/response protocol used
/// by external clients. This is the fire-and-forget path: there is no reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub target: Addressee,
    /// Set for `send-ref`, so the receiver can correlate a reply.
    pub ref_id: Option<u64>,
    pub payload: Vec<u8>,
}

/// Everything that crosses a peer link.
///
/// Control messages travel the same ordered stream as data, so a link request
/// cannot overtake a message sent before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PeerMessage {
    Deliver(Envelope),
    /// `from` (on the sending node) wants to link to `to` (on the receiving one).
    Link {
        from: Pid,
        to: Addressee,
    },
    Unlink {
        from: Pid,
        to: Addressee,
    },
    Monitor {
        watcher: Pid,
        target: Addressee,
        ref_id: u64,
    },
    Demonitor {
        watcher: Pid,
        ref_id: u64,
    },
    /// A linked particle died on the sending node.
    Exit {
        from: Pid,
        to: Pid,
        reason: ExitReason,
    },
    /// A monitored particle died on the sending node.
    Down {
        from: Pid,
        to: Pid,
        ref_id: u64,
        reason: ExitReason,
    },
}

/// Frames are length-prefixed so many can share one stream.
///
/// Matches the cap on the request/response path. Mailboxes are unbounded, so a
/// peer that could send arbitrarily large frames could exhaust memory on the
/// receiving node with a single message.
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;

pub fn encode_frame(msg: &PeerMessage) -> Result<Vec<u8>> {
    let body = wire::serialize(msg)?;
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
pub async fn read_frame(recv: &mut iroh::endpoint::RecvStream) -> Result<Option<PeerMessage>> {
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
    /// Published when a peer connection ends, so relationships crossing to it
    /// can fire. See [`PeerLoss`].
    pub loss: Arc<PeerLoss>,
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
            loss: Arc::new(PeerLoss::new()),
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
    pub fn send(&self, node: EndpointId, msg: &PeerMessage) -> Result<()> {
        let frame = encode_frame(msg)?;
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

        tokio::spawn(write_to_peer(
            self.endpoint.clone(),
            node,
            rx,
            self.loss.clone(),
        ));
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
    loss: Arc<PeerLoss>,
) {
    let mut stream: Option<iroh::endpoint::SendStream> = None;

    while let Some(frame) = frames.recv().await {
        // Up to two attempts: the first may find a stream that died since the
        // last write, and reconnecting once is enough to tell.
        for attempt in 0..2 {
            if stream.is_none() {
                match open_link(&endpoint, node, &loss).await {
                    Ok(s) => stream = Some(s),
                    Err(e) => {
                        tracing::debug!(peer = %node.fmt_short(), error = %e, "Could not reach peer; message dropped");
                        loss.announce_lost(node);
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
                        loss.announce_lost(node);
                    }
                }
            }
        }
    }

    tracing::debug!(peer = %node.fmt_short(), "Peer writer stopped");
}

async fn open_link(
    endpoint: &Endpoint,
    node: EndpointId,
    loss: &Arc<PeerLoss>,
) -> Result<iroh::endpoint::SendStream> {
    let conn = endpoint
        .connect(node, PLASMOID_ALPN)
        .await
        .context("failed to connect to peer")?;
    let stream = conn.open_uni().await.context("failed to open peer link")?;
    tracing::debug!(peer = %node.fmt_short(), "Opened peer link");

    // The QUIC idle timeout decides when a silent peer is gone; this resolves
    // when it trips, and immediately on an explicit close.
    let loss = loss.clone();
    tokio::spawn(async move {
        let reason = conn.closed().await;
        tracing::info!(peer = %node.fmt_short(), %reason, "Peer connection ended");
        loss.announce_lost(node);
    });

    Ok(stream)
}

/// Watch a peer for loss, so relationships crossing to it can be torn down.
///
/// The QUIC idle timeout is the authority per [#17]: keep-alive plus
/// `max_idle_timeout` decide when a silent peer is gone, and `Connection::closed`
/// resolves when it trips — which also catches an explicit close immediately.
/// Reimplementing this above the transport would duplicate what QUIC already
/// does correctly, which is why Erlang's `net_ticktime` has no counterpart here.
///
/// [#17]: https://github.com/scrogson/plasmoid/issues/17
pub struct PeerLoss {
    lost: tokio::sync::broadcast::Sender<EndpointId>,
}

impl Default for PeerLoss {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerLoss {
    pub fn new() -> Self {
        Self {
            lost: tokio::sync::broadcast::channel(256).0,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EndpointId> {
        self.lost.subscribe()
    }

    pub fn announce_lost(&self, node: EndpointId) {
        let _ = self.lost.send(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::PidGenerator;
    use iroh::SecretKey;

    fn a_pid() -> Pid {
        PidGenerator::new(SecretKey::generate().public()).next()
    }

    fn deliver(payload: &[u8]) -> PeerMessage {
        PeerMessage::Deliver(Envelope {
            target: Addressee::Pid(a_pid()),
            ref_id: None,
            payload: payload.to_vec(),
        })
    }

    #[test]
    fn test_frame_carries_its_length() {
        let msg = deliver(b"hello");
        let frame = encode_frame(&msg).unwrap();

        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(
            len,
            frame.len() - 4,
            "the prefix must describe exactly the body that follows"
        );
        assert_eq!(wire::deserialize::<PeerMessage>(&frame[4..]).unwrap(), msg);
    }

    #[test]
    fn test_frames_concatenate_without_ambiguity() {
        // Many messages share one stream, so a reader must be able to split them
        // apart from the prefixes alone.
        let a = deliver(b"first");
        let b = PeerMessage::Link {
            from: a_pid(),
            to: Addressee::Name("counter".into()),
        };

        let mut buf = encode_frame(&a).unwrap();
        buf.extend_from_slice(&encode_frame(&b).unwrap());

        let len_a = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        let decoded_a: PeerMessage = wire::deserialize(&buf[4..4 + len_a]).unwrap();
        let rest = &buf[4 + len_a..];
        let len_b = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        let decoded_b: PeerMessage = wire::deserialize(&rest[4..4 + len_b]).unwrap();

        assert_eq!(decoded_a, a);
        assert_eq!(decoded_b, b);
    }

    #[test]
    fn test_control_messages_share_the_ordered_stream() {
        // Control and data travel the same link on purpose: a link request must
        // not be able to overtake a message sent before it.
        for msg in [
            deliver(b"data"),
            PeerMessage::Unlink {
                from: a_pid(),
                to: Addressee::Pid(a_pid()),
            },
            PeerMessage::Monitor {
                watcher: a_pid(),
                target: Addressee::Pid(a_pid()),
                ref_id: 3,
            },
            PeerMessage::Exit {
                from: a_pid(),
                to: a_pid(),
                reason: ExitReason::NoConnection,
            },
            PeerMessage::Down {
                from: a_pid(),
                to: a_pid(),
                ref_id: 9,
                reason: ExitReason::NoProc,
            },
        ] {
            let frame = encode_frame(&msg).unwrap();
            let decoded: PeerMessage = wire::deserialize(&frame[4..]).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn test_oversized_messages_are_refused_rather_than_sent() {
        let msg = PeerMessage::Deliver(Envelope {
            target: Addressee::Pid(a_pid()),
            ref_id: None,
            payload: vec![0u8; MAX_FRAME_LEN as usize + 1],
        });
        assert!(
            encode_frame(&msg).is_err(),
            "a frame the receiver would reject must not be queued"
        );
    }
}
