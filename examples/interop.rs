#[cfg(not(target_arch = "wasm32"))]
mod bin {
    use libp2p::{
        core::{self, upgrade::AuthenticationVersion, PeerId},
        identity, mplex, noise,
        ping::{Ping, PingConfig, PingEvent},
        swarm::SwarmBuilder,
        yamux, NetworkBehaviour, Swarm, Transport,
    };
    use libp2p_webrtc::WebRtcTransport;
    use std::time::Duration;

    pub(crate) fn mk_swarm() -> Swarm<MyBehaviour> {
        let identity = identity::Keypair::generate_ed25519();

        let peer_id = PeerId::from(identity.public());
        let transport = {
            let base = WebRtcTransport::new(peer_id, vec!["stun:stun.l.google.com:19302"]);
            let noise_keys = noise::Keypair::<noise::X25519Spec>::new()
                .into_authentic(&identity)
                .expect("Signing libp2p-noise static DH keypair failed.");

            base.upgrade()
                .authenticate_with_version(
                    noise::NoiseConfig::xx(noise_keys).into_authenticated(),
                    AuthenticationVersion::V1SimultaneousOpen,
                )
                .multiplex(core::upgrade::SelectUpgrade::new(
                    yamux::YamuxConfig::default(),
                    mplex::MplexConfig::default(),
                ))
                .timeout(std::time::Duration::from_secs(20))
                .boxed()
        };

        SwarmBuilder::new(
            transport,
            MyBehaviour {
                ping: Ping::new(
                    PingConfig::new()
                        .with_interval(Duration::from_secs(1))
                        .with_keep_alive(true),
                ),
            },
            peer_id,
        )
        .executor(Box::new(|f| {
            tokio::spawn(f);
        }))
        .build()
    }
    #[derive(Debug)]
    pub(crate) enum MyEvent {
        Ping(PingEvent),
    }

    impl From<PingEvent> for MyEvent {
        fn from(event: PingEvent) -> Self {
            MyEvent::Ping(event)
        }
    }

    #[derive(NetworkBehaviour)]
    #[behaviour(event_process = false)]
    #[behaviour(out_event = "MyEvent")]
    pub(crate) struct MyBehaviour {
        ping: Ping,
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use libp2p::{
        futures::StreamExt,
        ping::{PingEvent, PingSuccess},
        swarm::SwarmEvent,
    };

    use log::*;
    use tracing_subscriber::fmt;

    use crate::bin::{mk_swarm, MyEvent};
    fmt::init();
    let mut swarm_0 = mk_swarm();
    let peer_0 = *swarm_0.local_peer_id();
    println!("/p2p/{}", peer_0);

    // /ip4/ws_signaling_ip/tcp/ws_signaling_port/{ws,wss}/p2p-webrtc-star/p2p/remote_peer_id
    swarm_0
        .listen_on(
            "/ip4/127.0.0.1/tcp/8001/ws/p2p-webrtc-star"
                .parse()
                .unwrap(),
        )
        .unwrap();

    while let Some(event) = swarm_0.next().await {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                info!("Listening on {}", address);
            }

            SwarmEvent::Behaviour(MyEvent::Ping(PingEvent {
                peer,
                result: Ok(PingSuccess::Ping { rtt }),
            })) => {
                info!("Ping to {} is {}ms", peer, rtt.as_millis());
                break;
            }
            other => {
                info!("Unhandled {:?}", other);
            }
        }
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn main() {
    panic!("not supported on wasm32");
}
