#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use libp2p::{
        core::{self, transport::upgrade, PeerId},
        futures::{future, FutureExt, StreamExt},
        identity, mplex, noise,
        ping::{Ping, PingConfig, PingEvent, PingSuccess},
        swarm::{SwarmBuilder, SwarmEvent},
        yamux, NetworkBehaviour, Swarm, Transport,
    };
    use libp2p_webrtc::WebRtcTransport;
    use log::*;
    use std::{
        collections::BTreeSet,
        process::Stdio,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::{Child, Command},
        time::timeout,
    };
    use tracing_subscriber::fmt;

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
            tokio::spawn(f);
        }))
        .build()
    }

    async fn start_signaling_server() -> anyhow::Result<Child> {
        let mut cmd = Command::new("cargo");
        cmd.args(&["run", "--", "--interface", "127.0.0.1"])
            .current_dir("../signal")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut server = cmd.spawn()?;
        let stdout = server.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout).lines();
        while let Some(line) = reader.next_line().await? {
            if line.starts_with("Listening on") {
                break;
            }
        }
        println!("Signaling server started!");
        server.stdout.replace(reader.into_inner().into_inner());
        Ok(server)
    }

    #[tokio::test]
    async fn native_native() -> anyhow::Result<()> {
        let _ = fmt::try_init();
        {
            // wait for server init
            let mut _server = start_signaling_server().await?;
            native_native_single().await?;
            native_native_concurrent().await?;
        }
        {
            // late server init
            future::try_join3(
                async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    start_signaling_server().await
                },
                native_native_single(),
                native_native_concurrent(),
            )
            .await?;
        }
        Ok(())
    }

    async fn native_native_single() -> anyhow::Result<()> {
        let mut swarm_0 = mk_swarm();
        let peer_0 = *swarm_0.local_peer_id();
        info!("Local peer id 0: {}", peer_0);
        let mut swarm_1 = mk_swarm();
        let peer_1 = *swarm_1.local_peer_id();
        info!("Local peer id 1: {}", peer_1);

        // /ip4/ws_signaling_ip/tcp/ws_signaling_port/{ws,wss}/p2p-webrtc-star/p2p/remote_peer_id
        swarm_0
            .listen_on(
                "/ip4/127.0.0.1/tcp/8000/ws/p2p-webrtc-star"
                    .parse()
                    .unwrap(),
            )
            .unwrap();

        swarm_1
            .listen_on(
                "/ip4/127.0.0.1/tcp/8000/ws/p2p-webrtc-star"
                    .parse()
                    .unwrap(),
            )
            .unwrap();

        let s0 = async move {
            while let Some(event) = swarm_0.next().await {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!("Listening on {}", address);

                        swarm_0.dial_addr(
                            format!("/ip4/127.0.0.1/tcp/8000/ws/p2p-webrtc-star/p2p/{}", peer_1)
                                .parse()?,
                        )?;
                    }

                    SwarmEvent::Behaviour(MyEvent::Ping(PingEvent {
                        peer,
                        result: Ok(PingSuccess::Ping { rtt }),
                    })) if peer == peer_1 => {
                        info!("Ping to {} is {}ms", peer, rtt.as_millis());
                        return Ok(swarm_0);
                    }
                    other => {
                        info!("Unhandled {:?}", other);
                    }
                }
            }
            anyhow::Result::<_, anyhow::Error>::Ok(swarm_0)
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
                        return Ok(swarm_1);
                    }
                    other => {
                        info!("Unhandled {:?}", other);
                    }
                }
            }
            anyhow::Result::<_, anyhow::Error>::Ok(swarm_1)
        };
        timeout(Duration::from_secs(10), future::try_join(s0, s1)).await??;
        Ok(())
    }

    async fn native_native_concurrent() -> anyhow::Result<()> {
        info!("Signaling server started!");
        let mut listener = mk_swarm();
        let peer_0 = *listener.local_peer_id();
        info!("Local peer id 0: {}", peer_0);

        // /ip4/ws_signaling_ip/tcp/ws_signaling_port/{ws,wss}/p2p-webrtc-star/p2p/remote_peer_id
        listener
            .listen_on(
                "/ip4/127.0.0.1/tcp/8000/ws/p2p-webrtc-star"
                    .parse()
                    .unwrap(),
            )
            .unwrap();
        let listener_connected = Arc::new(AtomicBool::new(false));
        let connected = listener_connected.clone();

        const NUM_DIALERS: usize = 10;
        let s0 = async move {
            let mut dialed = BTreeSet::new();
            while let Some(event) = listener.next().await {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!("Listening on {}", address);
                        connected.store(true, Ordering::Relaxed);
                    }

                    SwarmEvent::Behaviour(MyEvent::Ping(PingEvent {
                        peer,
                        result: Ok(PingSuccess::Ping { rtt }),
                    })) => {
                        info!("Ping to {} is {}ms", peer, rtt.as_millis());
                        dialed.insert(peer);
                        if dialed.len() >= NUM_DIALERS {
                            return Ok(listener);
                        }
                    }
                    other => {
                        info!("Unhandled {:?}", other);
                    }
                }
            }
            anyhow::Result::<_, anyhow::Error>::Ok(listener)
        }
        .boxed();
        let dialers = (0..NUM_DIALERS).map(|_| mk_swarm()).map(move |mut swarm| {
            let connected = listener_connected.clone();
            async move {
                while !connected.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                swarm.dial_addr(
                    format!("/ip4/127.0.0.1/tcp/8000/ws/p2p-webrtc-star/p2p/{}", peer_0).parse()?,
                )?;
                while let Some(event) = swarm.next().await {
                    match event {
                        SwarmEvent::NewListenAddr { address, .. } => {
                            info!("Listening on {}", address);
                        }

                        SwarmEvent::Behaviour(MyEvent::Ping(PingEvent {
                            peer,
                            result: Ok(PingSuccess::Ping { rtt }),
                        })) if peer == peer_0 => {
                            info!("Ping to {} is {}ms", peer, rtt.as_millis());
                            return Ok(swarm);
                        }
                        other => {
                            info!("Unhandled {:?}", other);
                        }
                    }
                }
                anyhow::Result::<_, anyhow::Error>::Ok(swarm)
            }
            .boxed()
        });

        timeout(
            Duration::from_secs(10),
            future::try_join_all(std::iter::once(s0).chain(dialers)),
        )
        .await??;
        Ok(())
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
