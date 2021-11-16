#[cfg(target_arch = "wasm32")]
mod tests {
    use libp2p::{
        core::{self, transport::upgrade, PeerId},
        futures::{future, StreamExt},
        identity, mplex, noise,
        ping::{Ping, PingConfig, PingEvent, PingSuccess},
        swarm::{SwarmBuilder, SwarmEvent},
        yamux, Multiaddr, NetworkBehaviour, Swarm, Transport,
    };
    use libp2p_webrtc::WebRtcTransport;
    use log::*;
    use std::time::Duration;
    use wasm_bindgen_futures::spawn_local;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    fn mk_swarm() -> Swarm<MyBehaviour> {
        let identity = identity::Keypair::generate_ed25519();

        let peer_id = PeerId::from(identity.public());
        let transport = {
            let base = WebRtcTransport::new(peer_id, vec!["stun:stun.l.google.com:19302"]);
            let noise_keys = noise::Keypair::<noise::X25519Spec>::new()
                .into_authentic(&identity)
                .expect("Signing libp2p-noise static DH keypair failed.");

            base.upgrade(upgrade::Version::V1Lazy)
                .authenticate(noise::NoiseConfig::xx(noise_keys).into_authenticated())
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
            spawn_local(f);
        }))
        .build()
    }

    #[wasm_bindgen_test]
    async fn wasm_wasm() {
        console_log::init_with_level(Level::Trace).unwrap();

        // make sure signaling server is started
        let mut swarm_0 = mk_swarm();
        let peer_0 = *swarm_0.local_peer_id();
        info!("Local peer id 0: {}", peer_0);
        let mut swarm_1 = mk_swarm();
        let peer_1 = *swarm_1.local_peer_id();
        info!("Local peer id 1: {}", peer_1);

        // /ip4/ws_signaling_ip/tcp/ws_signaling_port/{ws,wss}/p2p-webrtc-star/p2p/remote_peer_id
        swarm_0
            .listen_on(
                "/ip4/127.0.0.1/tcp/8001/ws/p2p-webrtc-star"
                    .parse()
                    .unwrap(),
            )
            .unwrap();

        swarm_1
            .listen_on(
                "/ip4/127.0.0.1/tcp/8001/ws/p2p-webrtc-star"
                    .parse()
                    .unwrap(),
            )
            .unwrap();

        let s0 = async move {
            swarm_0.dial(
                format!("/ip4/127.0.0.1/tcp/8001/ws/p2p-webrtc-star/p2p/{}", peer_1)
                    .parse::<Multiaddr>()?,
            )?;

            while let Some(event) = swarm_0.next().await {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!("Listening on {}", address);
                    }

                    SwarmEvent::Behaviour(MyEvent::Ping(PingEvent {
                        peer,
                        result: Ok(PingSuccess::Ping { rtt }),
                    })) if peer == peer_1 => {
                        info!("Ping to {} is {}ms", peer, rtt.as_millis());
                        return Ok(());
                    }
                    other => {
                        info!("Unhandled {:?}", other);
                    }
                }
            }
            anyhow::Result::<_, anyhow::Error>::Ok(())
        };
        let s1 = async move {
            while let Some(event) = swarm_1.next().await {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!("Listening on {}", address);
                    }

                    SwarmEvent::Behaviour(MyEvent::Ping(PingEvent {
                        peer,
                        result: Ok(PingSuccess::Ping { rtt }),
                    })) if peer == peer_0 => {
                        info!("Ping to {} is {}ms", peer, rtt.as_millis());
                        return Ok(());
                    }
                    other => {
                        info!("Unhandled {:?}", other);
                    }
                }
            }
            anyhow::Result::<_, anyhow::Error>::Ok(())
        };

        future::try_join(s0, s1).await.unwrap();
    }

    #[derive(Debug)]
    enum MyEvent {
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
    struct MyBehaviour {
        ping: Ping,
    }
}
