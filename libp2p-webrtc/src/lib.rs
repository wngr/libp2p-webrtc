#![allow(clippy::let_and_return)]
use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use crate::ws::{CombinedStream, Message};
use anyhow::Context;
#[cfg(not(target_arch = "wasm32"))]
use async_datachannel::{DataStream, Message as DataChannelMessage, PeerConnection, RtcConfig};
#[cfg(target_arch = "wasm32")]
use async_datachannel_wasm::{
    DataStream, Message as DataChannelMessage, PeerConnection, RtcConfig,
};
use async_stream::try_stream;
use libp2p::{
    core::transport::ListenerEvent,
    futures::{
        channel::mpsc,
        future::BoxFuture,
        pin_mut, select, select_biased,
        stream::{BoxStream, FuturesUnordered},
        FutureExt, SinkExt, StreamExt, TryFutureExt,
    },
    multiaddr::Protocol,
    Multiaddr, PeerId, Transport,
};
use log::*;
#[cfg(target_arch = "wasm32")]
use send_wrapper::SendWrapper;
use serde::{Deserialize, Serialize};
use streamunordered::{StreamUnordered, StreamYield};
use thiserror::Error;

mod ws;

#[derive(Clone)]
pub struct WebRtcTransport {
    config: RtcConfig,
    id: Arc<AtomicUsize>,
    own_peer_id: PeerId,
}
// Uniquely identify a signaling request. PeerId is the initiator's peer id and a counter.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Eq, Hash, Ord, PartialOrd)]
pub struct SignalingId {
    #[serde(with = "serde_str")]
    pub caller: PeerId,
    pub counter: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalingMessage {
    pub intent_id: SignalingId,
    #[serde(with = "serde_str")]
    pub callee: PeerId,
    pub signal: DataChannelMessage,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Connection to signaling server failed: {0}")]
    ConnectionToSignalingServerFailed(String),
    #[error("Unexpected Message: {0}")]
    UnexpectedMessage(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl WebRtcTransport {
    pub fn new(peer_id: PeerId, ice_servers: Vec<&str>) -> Self {
        let config = RtcConfig::new(&ice_servers[..]);

        Self {
            config,
            id: Default::default(),
            own_peer_id: peer_id,
        }
    }
}

#[allow(clippy::type_complexity)]
impl Transport for WebRtcTransport {
    type Output = DataStream;

    type Error = Error;

    type Listener =
        BoxStream<'static, Result<ListenerEvent<Self::ListenerUpgrade, Self::Error>, Self::Error>>;

    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn listen_on(
        self,
        addr: libp2p::Multiaddr,
    ) -> Result<Self::Listener, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        debug!("called listen on with {}", addr);
        let (signaling_uri, maybe_p2p) = extract_uri(&addr)
            .map_err(|_| libp2p::TransportError::MultiaddrNotSupported(addr.clone()))?;
        if maybe_p2p.is_some() {
            return Err(libp2p::TransportError::MultiaddrNotSupported(addr));
        }
        let signaling_uri = format!("{}/{}", signaling_uri, self.own_peer_id);
        // input addr
        // /ip4/ws_signaling_ip/tcp/ws_signaling_port/{ws,wss}/p2p-webrtc-star/p2p/remote_peer_id
        let s = try_stream! {
            let (mut ws_tx, ws_rx) = CombinedStream::connect(&signaling_uri).await?.split();
            yield ListenerEvent::NewAddress(addr.clone());
            let mut ws_rx = ws_rx.fuse();

            // TODO
            // let mut backoff: Option<u64> = None;
            let mut pending_upgrades = FuturesUnordered::new();
            let mut open_upgrades =
                BTreeMap::<SignalingId, mpsc::Sender<DataChannelMessage>>::new();
            let mut outbound = StreamUnordered::new();
            loop {
                select! {
                    from_ws = ws_rx.next() => {
                        let message = match from_ws {
                           Some(Ok(Message::Text(t))) => {
                               serde_json::from_str::<SignalingMessage>(&t)
                           },
                           Some(Ok(Message::Binary(b))) => {
                               serde_json::from_slice::<SignalingMessage>(&b[..])
                           },
                           _ => {
                               info!("Address expired {}", addr);
                               yield ListenerEvent::AddressExpired(addr.clone());
                               return;
                           }
                        };
                        match message {
                          Ok(SignalingMessage { intent_id, callee, signal }) => {
                            if callee != self.own_peer_id {
                                yield ListenerEvent::Error(Error::UnexpectedMessage(format!("Message for {}", callee)));
                                continue;
                            }
                            debug!("Received {:?} {:?}", intent_id, signal);
                            if let Some(tx) = open_upgrades.get_mut(&intent_id) {
                                let _ = tx.start_send(signal);
                            } else {
                                let (tx_outbound, rx_outbound) = mpsc::channel(8);
                                let (mut tx_inbound, rx_inbound) = mpsc::channel(8);
                                let id = intent_id.clone();
                                outbound.insert(
                                    rx_outbound.map(move |m| (id.clone(), m))
                                    );
                                let conn =
                                    PeerConnection::new(&self.config, (tx_outbound, rx_inbound)).map_err(Error::Internal).unwrap();
                                tx_inbound.start_send(signal).expect("Channel open");
                                open_upgrades.insert(intent_id.clone(), tx_inbound);
                                let u = conn.accept().map(move |r| (intent_id, r));
                                #[cfg(target_arch = "wasm32")]
                                let u = SendWrapper::new(u);
                                pending_upgrades.push(u.boxed());
                              }
                          },
                          Err(e) => {
                            error!("Error deserializing message {:?}", e);
                          }
                        }
                    },
                    (id, maybe_upgrade) = pending_upgrades.select_next_some() => {
                        open_upgrades.remove(&id);
                        yield ListenerEvent::Upgrade {
                            upgrade: async move { maybe_upgrade.map_err(Into::into) }.boxed(),
                            remote_addr: addr.clone().with(Protocol::P2p(id.caller.into())),
                            local_addr: addr.clone().with(Protocol::P2p(self.own_peer_id.into()))
                        };
                    },
                    (item, token) = outbound.select_next_some() => {
                        match item {
                            StreamYield::Item((intent_id, signal)) => {
                                let m = SignalingMessage {
                                    intent_id,
                                    signal,
                                    callee: self.own_peer_id
                                };
                                let bytes = serde_json::to_vec(&m).map_err(|e| Error::Internal(e.into())).unwrap();
                                let _ = ws_tx.send(Message::Binary(bytes)).await;
                            }
                            _ => {
                                Pin::new(&mut outbound).remove(token);
                            }
                        }
                    }
                }
            }
        };
        #[cfg(target_arch = "wasm32")]
        let s = SendWrapper::new(s);
        Ok(s.boxed())
    }

    fn dial(
        self,
        addr: libp2p::Multiaddr,
    ) -> Result<Self::Dial, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        let (signaling_uri, peer) = extract_uri(&addr)
            .ok()
            .and_then(|(s, p)| p.map(|p| (s, p)))
            .ok_or(libp2p::TransportError::MultiaddrNotSupported(addr))?;
        let counter = self.id.fetch_add(1, Ordering::Relaxed);
        let signaling_uri = format!("{}/{}/{}", signaling_uri, self.own_peer_id, counter);

        let (tx_outbound, mut rx_outbound) = mpsc::channel(32);
        let (mut tx_inbound, rx_inbound) = mpsc::channel(32);
        let conn = PeerConnection::new(&self.config, (tx_outbound, rx_inbound))
            .map_err(|e| libp2p::TransportError::Other(e.into()))?;
        let identifier = SignalingId {
            caller: self.own_peer_id,
            counter,
        };

        let fut = async move {
            let (mut ws_tx, ws_rx) = CombinedStream::connect(&signaling_uri).await?.split();
            let connection = conn.dial("unused").into_stream().fuse();
            let mut ws_rx = ws_rx.fuse();
            pin_mut!(connection);
            loop {
                select_biased! {
                    conn = connection.next() => {
                        let conn = conn.context("Stream ended")?;
                        debug!("dial: created data stream");
                        break conn;
                    },
                    incoming_ws = ws_rx.next() => {
                        let incoming_ws = incoming_ws.context("Stream ended")?;
                        debug!("dial: received message {:?}", incoming_ws);
                        let message = match incoming_ws {
                           Ok(Message::Text(t)) => {
                               Some(serde_json::from_str::<SignalingMessage>(&t))
                           },
                           Ok(Message::Binary(b)) => {
                               Some(serde_json::from_slice::<SignalingMessage>(&b[..]))
                           },
                           Ok(Message::Close) => None,
                           x => anyhow::bail!("Connection to signaling server closed ({:?})", x)
                        };
                        match message {
                            Some(Ok(m)) if m.intent_id == identifier => tx_inbound.send(m.signal).await?,
                            Some(Ok(m)) => error!("Received message with unexpected identifier {:?}", m.intent_id),
                            Some(Err(e)) => error!("Error ws_rxing from WS: {:?}", e),
                            _ => {},
                        }
                    },
                    signal = rx_outbound.next() => {
                        let signal = signal.context("Stream ended")?;
                        let m = SignalingMessage {
                            intent_id: identifier.clone(),
                            callee: peer,
                            signal,
                        };
                        debug!("dial: sending message {:?}", m);
                        let bytes = serde_json::to_vec(&m)?;
                        ws_tx.send(Message::Binary(bytes)).await?;
                    },
                }
            }
        };
        #[cfg(target_arch = "wasm32")]
        let fut = SendWrapper::new(fut);
        Ok(fut.map_err(Into::into).boxed())
    }

    fn address_translation(
        &self,
        _listen: &libp2p::Multiaddr,
        _observed: &libp2p::Multiaddr,
    ) -> Option<libp2p::Multiaddr> {
        // TODO?
        None
    }
}

pub mod serde_str {
    //! Serializes fields annotated with `#[serde(with = "::util::serde_str")]` with their !
    //! `Display` implementation, deserializes fields using `FromStr`.
    use std::fmt::Display;
    use std::str::FromStr;

    use serde::{de, Deserialize, Deserializer, Serializer};

    pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Display,
        S: Serializer,
    {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
    where
        T: FromStr,
        T::Err: Display,
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

// /ip4/ws_signaling_ip/tcp/ws_signaling_port/{ws,wss}/p2p-webrtc-star/p2p/remote_peer_id
fn extract_uri(addr: &Multiaddr) -> anyhow::Result<(String, Option<PeerId>)> {
    anyhow::ensure!(
        addr.iter().any(|p| p == Protocol::P2pWebRtcStar),
        "Only p2p-webrtc-star connections are supported."
    );
    let mut protocol = None;
    let mut host = None;
    let mut port = None;
    let mut peer = None;
    for i in addr {
        if match i {
            Protocol::Dns4(h) | Protocol::Dns6(h) => host.replace(h.to_string()).is_some(),
            Protocol::Ip4(h) => host.replace(h.to_string()).is_some(),
            Protocol::Ip6(h) => host.replace(h.to_string()).is_some(),
            Protocol::P2p(p) => peer
                .replace(
                    PeerId::from_multihash(p).map_err(|e| anyhow::anyhow!(format!("{:?}", e)))?,
                )
                .is_some(),
            Protocol::Tcp(p) => port.replace(p).is_some(),
            Protocol::Ws(_) => protocol.replace("ws".to_string()).is_some(),
            Protocol::Wss(_) => protocol.replace("wss".to_string()).is_some(),

            _ => false,
        } {
            anyhow::bail!("Unexpected format: {}", addr)
        }
    }
    if let (Some(protocol), Some(host), Some(port)) = (protocol, host, port) {
        Ok((format!("{}://{}:{}", protocol, host, port), peer))
    } else {
        anyhow::bail!("Unable to extract signaling uri and peer from {}", addr)
    }
}
