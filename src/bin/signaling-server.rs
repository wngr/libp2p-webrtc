#[cfg(not(target_arch = "wasm32"))]
mod bin {
    use std::{collections::HashMap, str::FromStr, sync::Arc};

    use async_tungstenite::{
        tokio::accept_hdr_async,
        tungstenite::{
            handshake::server::{ErrorResponse, Request, Response},
            Message,
        },
    };
    use libp2p::{
        futures::{future, pin_mut, StreamExt},
        PeerId,
    };
    use libp2p_webrtc::SignalingMessage;
    use parking_lot::Mutex;
    use tokio::{net::TcpStream, sync::mpsc};
    use tokio_stream::wrappers::ReceiverStream;

    type Tx = mpsc::Sender<Message>;
    type ClientsMap = Arc<Mutex<HashMap<(PeerId, Option<usize>), Tx>>>;

    pub(crate) async fn handle(clients: ClientsMap, stream: TcpStream) -> anyhow::Result<()> {
        let mut client_id = None;
        let callback = |req: &Request, response: Response| {
            let mut tokens = req.uri().path().split('/');
            if let Some(token) = tokens.nth(1) {
                match PeerId::from_str(token) {
                    Ok(p) => {
                        let id = tokens.next().and_then(|x| x.parse().ok());
                        client_id.replace((p, id));
                        Ok(response)
                    }
                    Err(e) => Err(ErrorResponse::new(Some(format!(
                        "\"{:?}\" is not a valid PeerId ({})",
                        token, e
                    )))),
                }
            } else {
                Err(ErrorResponse::new(Some("Missing client id".to_string())))
            }
        };

        let websocket = accept_hdr_async(stream, callback).await?;
        let client_id = client_id.expect("Callback called");
        println!("Client {:?} connected", &client_id);

        let (tx, rx) = mpsc::channel(128);
        clients.lock().insert(client_id, tx);

        let (outgoing, mut incoming) = websocket.split();
        let forward = ReceiverStream::new(rx).map(Ok).forward(outgoing);
        let c = clients.clone();
        let process = async move {
            while let Some(Ok(msg)) = incoming.next().await {
                let maybe_signal = match &msg {
                    Message::Text(t) => serde_json::from_str::<SignalingMessage>(t).ok(),
                    Message::Binary(b) => serde_json::from_slice::<SignalingMessage>(b).ok(),
                    _ => None,
                };
                if let Some(signal) = maybe_signal {
                    let to = if client_id.0 == signal.callee {
                        // answer
                        (signal.intent_id.caller, Some(signal.intent_id.counter))
                    } else {
                        // offer
                        (signal.callee, None)
                    };
                    // Can't hold onto mutex across an await point
                    let maybe_remote = c.lock().get(&to).cloned();
                    if let Some(remote) = maybe_remote {
                        println!("{:?} << {:?}", to, signal);
                        if let Err(e) = remote.clone().send(msg).await {
                            println!("Error sending to {:?}: {:?}", to, e);
                        }
                    } else {
                        println!("Received message for \"{:?}\", but client is unknown", to);
                    }
                };
            }
        };

        pin_mut!(process, forward);
        future::select(process, forward).await;

        println!("Client {:?} disconnected", &client_id);
        clients.lock().remove(&client_id);
        Ok(())
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    use std::{collections::HashMap, env, sync::Arc};

    use parking_lot::Mutex;
    use tokio::net::TcpListener;

    let service = env::args().nth(1).unwrap_or_else(|| "8000".to_string());
    let endpoint = if service.contains(':') {
        service
    } else {
        format!("127.0.0.1:{}", service)
    };

    println!("Listening on {}", endpoint);

    let listener = TcpListener::bind(endpoint).await?;
    let clients = Arc::new(Mutex::new(HashMap::new()));

    while let Ok((stream, _)) = listener.accept().await {
        let c = clients.clone();
        tokio::spawn(async move {
            if let Err(e) = bin::handle(c, stream).await {
                println!("Error handling inbound stream: {:#}", e);
            }
        });
    }

    Ok(())
}
#[cfg(target_arch = "wasm32")]
fn main() {
    panic!("Not supported on wasm32 target");
}
