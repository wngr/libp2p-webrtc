use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_datachannel::{DataStream, PeerConnection, RtcConfig};
use async_stream::try_stream;
use async_tungstenite::{tokio::connect_async, tungstenite::Message};
use libp2p::{
    core::transport::ListenerEvent,
    futures::{
        future::BoxFuture, pin_mut, stream::BoxStream, FutureExt, SinkExt, StreamExt, TryFutureExt,
    },
    multiaddr::Protocol,
    Multiaddr, PeerId, Transport,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};
use tracing::{debug, error};

#[derive(Clone)]
pub struct WebRtcTransport {
    config: RtcConfig,
    id: Arc<AtomicUsize>,
    own_peer_id: PeerId,
}
// Uniquely identify a signaling request. PeerId is the initiator's peer id and a counter.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Eq, Hash)]
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
    pub signal: async_datachannel::Message,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Whatever")]
    Whatever(String),
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
    type Output = Compat<DataStream>;

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
        println!("called listen on with {}", addr);
        let (signaling_uri, _) = extract_uri(&addr)
            .map_err(|_| libp2p::TransportError::MultiaddrNotSupported(addr.clone()))?;
        let signaling_uri = format!("{}/{}", signaling_uri, self.own_peer_id);
        let local_addr = addr.clone().with(Protocol::P2p(self.own_peer_id.into()));
        // input addr
        // /ip4/ws_signaling_ip/tcp/ws_signaling_port/{ws,wss}/p2p-webrtc-star/p2p/remote_peer_id
        Ok(try_stream! {
            loop {
                let upgrade = self
                    .listen_single(&signaling_uri)
                    .await
                    .map_err(|e| Error::Whatever(format!("{:#}", e)));
                let remote_addr = if let Ok((p, _)) = upgrade {
                    addr.clone().with(Protocol::P2p(p.into()))
                } else {
                    addr.clone()
                };

                yield ListenerEvent::Upgrade {
                    upgrade: async move { upgrade.map(|(_, u)| u.compat()) }.boxed(),
                    remote_addr,
                    local_addr: local_addr.clone()
                };
            }
        }
        .boxed())
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
        let (tx_inbound, rx_inbound) = mpsc::channel(32);
        let conn = PeerConnection::new(&self.config, (tx_outbound, rx_inbound))
            .map_err(|e| libp2p::TransportError::Other(Error::Whatever(format!("{:#}", e))))?;
        let identifier = SignalingId {
            caller: self.own_peer_id,
            counter,
        };

        let fut = async move {
            let (mut ws_tx, mut ws_rx) = connect_async(&signaling_uri).await?.0.split();
            let connection = conn.dial("unused").into_stream();
            pin_mut!(connection);
            loop {
                tokio::select! {
                    biased;

                    Some(conn) = connection.next() => {
                        debug!("dial: created data stream");
                        break conn.map(|c| c.compat());
                    },
                    Some(incoming_ws) = ws_rx.next() => {
                        debug!("dial: received message {:?}", incoming_ws);
                        let message = match incoming_ws {
                           Ok(Message::Text(t)) => {
                               Some(serde_json::from_str::<SignalingMessage>(&t))
                           },
                           Ok(Message::Binary(b)) => {
                               Some(serde_json::from_slice::<SignalingMessage>(&b[..]))
                           },
                           Ok(Message::Close(_)) => None,
                           x => anyhow::bail!("Connection to signaling server closed ({:?})", x)
                        };
                        match message {
                            Some(Ok(m)) if m.intent_id == identifier => tx_inbound.send(m.signal).await?,
                            Some(Ok(m)) => error!("Received message with unexpected identifier {:?}", m.intent_id),
                            Some(Err(e)) => error!("Error ws_rxing from WS: {:?}", e),
                            _ => {},
                        }
                    },
                    Some(signal) = rx_outbound.recv() => {
                        let m = SignalingMessage {
                            intent_id: identifier.clone(),
                            callee: peer,
                            signal,
                        };
                        debug!("dial: sending message {:?}", m);
                        let bytes = serde_json::to_vec(&m)?;
                        ws_tx.send(Message::binary(bytes)).await?;
                    },
                    else => anyhow::bail!("FIXME"),
                }
            }
        }
        .map_err(|e| Error::Whatever(format!("{:#}", e)))
        .boxed();
        Ok(fut)
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
impl WebRtcTransport {
    async fn listen_single(&self, signaling_uri: &str) -> anyhow::Result<(PeerId, DataStream)> {
        println!("connecting to {}", signaling_uri);
        let (mut ws_tx, mut ws_rx) = connect_async(signaling_uri).await?.0.split();
        println!("connected to {}", signaling_uri);

        let (tx_outbound, mut rx_outbound) = mpsc::channel(32);
        let (tx_inbound, rx_inbound) = mpsc::channel(32);
        let conn = PeerConnection::new(&self.config, (tx_outbound, rx_inbound))
            .map_err(|e| Error::Whatever(format!("{:#}", e)))?;

        let upgrade = conn.accept().into_stream();
        pin_mut!(upgrade);
        let mut identifier = None;
        let io = loop {
            tokio::select! {
                Some(conn) = upgrade.next() => {
                    debug!("listen: created data stream");
                    break conn?;
                },
                Some(incoming_ws) = ws_rx.next() => {
                    debug!("listen: received message {:?}", incoming_ws);
                    let message = match incoming_ws {
                       Ok(Message::Text(t)) => {
                           Some(serde_json::from_str::<SignalingMessage>(&t))
                       },
                       Ok(Message::Binary(b)) => {
                           Some(serde_json::from_slice::<SignalingMessage>(&b[..]))
                       },
                       Ok(Message::Close(_)) => None,
                       x => anyhow::bail!("Connection to signaling server closed ({:?})", x)
                    };
                    match message {
                        Some(Ok(m)) if identifier.is_none() => {
                            debug!("Inbound connection with {:?}", m.intent_id);
                            identifier.replace(m.intent_id);
                            tx_inbound.send(m.signal).await?;
                        },
                        Some(Ok(m)) if identifier.as_ref() == Some(&m.intent_id) => {
                            tx_inbound.send(m.signal).await?;
                        },
                        Some(Ok(m)) => error!("Received message with unexpected identifier {:?}", m.intent_id),
                        Some(Err(e)) => error!("Error ws_rxing from WS: {:?}", e),
                        None => {},
                    }
                },
                Some(signal) = rx_outbound.recv() => {
                    let m = SignalingMessage {
                        intent_id: identifier.as_ref().cloned().expect("Sending message before received one"),
                        callee: self.own_peer_id,
                        signal,
                    };
                    debug!("listen: sending message {:?}", m);
                    let bytes = serde_json::to_vec(&m)?;
                    ws_tx.send(Message::binary(bytes)).await?;
                },
                else => anyhow::bail!("FIXME"),
            }
        };
        Ok((identifier.expect("Negotiation happened").caller, io))
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
