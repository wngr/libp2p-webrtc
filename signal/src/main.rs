use std::{
    collections::HashMap,
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_tungstenite::{
    stream::Stream,
    tokio::accept_hdr_async,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        Message,
    },
};
use clap::{AppSettings, Clap};
use futures::{channel::oneshot, future, pin_mut, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_rustls::{
    rustls::{
        internal::pemfile::{certs, pkcs8_private_keys},
        NoClientAuth, ServerConfig,
    },
    TlsAcceptor,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::compat::*;
use tracing::*;
use tracing_subscriber::fmt;
use warp::Filter;

type Tx = mpsc::Sender<Message>;
type ClientsMap = Arc<Mutex<HashMap<(String, Option<usize>), Tx>>>;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Eq, Hash)]
pub struct SignalingId {
    pub caller: String,
    pub counter: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalingMessage {
    pub intent_id: SignalingId,
    pub callee: String,
    pub signal: serde_json::Value,
}

async fn handle(
    clients: ClientsMap,
    stream: TcpStream,
    tls_acceptor: Option<TlsAcceptor>,
) -> anyhow::Result<()> {
    let stream = if let Some(acceptor) = tls_acceptor {
        let stream = acceptor.accept(stream).await?;
        Stream::Tls(stream.compat())
    } else {
        Stream::Plain(stream.compat())
    }
    .compat();
    let mut client_id = None;
    let callback = |req: &Request, response: Response| {
        let mut tokens = req.uri().path().split('/');
        if let Some(token) = tokens.nth(1) {
            let id = tokens.next().and_then(|x| x.parse().ok());
            client_id.replace((token.into(), id));
            Ok(response)
        } else {
            Err(ErrorResponse::new(Some("Missing client id".to_string())))
        }
    };

    let websocket = accept_hdr_async(stream, callback).await?;
    let client_id = client_id.expect("Callback called");
    println!("Client {:?} connected", &client_id);

    let (tx, rx) = mpsc::channel(128);
    clients.lock().insert(client_id.clone(), tx);

    let (outgoing, mut incoming) = websocket.split();
    let forward = ReceiverStream::new(rx).map(Ok).forward(outgoing);
    let c = clients.clone();
    let client_id_c = client_id.clone();
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
                    (
                        signal.intent_id.caller.clone(),
                        Some(signal.intent_id.counter),
                    )
                } else {
                    // offer
                    (signal.callee.clone(), None)
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

    println!("Client {:?} disconnected", &client_id_c);
    clients.lock().remove(&client_id_c);
    Ok(())
}

async fn get_cert(
    domain: String,
    email: String,
    certificate_file: impl AsRef<Path>,
    private_key_file: impl AsRef<Path>,
) -> anyhow::Result<()> {
    let url = acme_lib::DirectoryUrl::LetsEncrypt;

    let persist = acme_lib::persist::FilePersist::new(".");

    // Create a directory entrypoint.
    let dir = acme_lib::Directory::from_url(persist, url)?;

    let acc = dir.account(&email)?;

    let mut ord_new = acc.new_order(&domain, &[])?;
    let ord_csr = loop {
        // are we done?
        if let Some(ord_csr) = ord_new.confirm_validations() {
            break ord_csr;
        }

        let auths = ord_new.authorizations()?;
        let chall = auths[0].http_challenge();
        let token = chall.http_token().to_string();
        let proof = chall.http_proof();

        info!(%token, %proof, "Serving acme-challenge");
        let token = warp::get()
            .and(warp::path!(".well-known" / "acme-challenge" / String))
            .map(move |_| {
                info!("Challenge served.");
                proof.clone()
            });
        let (tx80, rx80) = oneshot::channel();
        tokio::spawn(
            warp::serve(token)
                .bind_with_graceful_shutdown(([0, 0, 0, 0], 80), async {
                    rx80.await.ok();
                })
                .1,
        );

        chall.validate(5000)?;
        info!("Validated!");
        tx80.send(()).unwrap();

        ord_new.refresh()?;
    };

    let pkey_pri = acme_lib::create_p384_key();
    let ord_cert = ord_csr.finalize_pkey(pkey_pri, 5000)?;

    let cert = ord_cert.download_and_save_cert()?;
    info!("Received certificate");

    std::fs::write(&certificate_file, cert.certificate())?;
    std::fs::write(&private_key_file, cert.private_key())?;
    info!(
        "Stored certificate / private key to {} / {}",
        certificate_file.as_ref().display(),
        private_key_file.as_ref().display()
    );
    Ok(())
}

#[derive(Clap)]
#[clap(setting = AppSettings::ColoredHelp)]
struct Opts {
    /// Interface to bind to
    #[clap(long)]
    interface: String,
    /// Domain to request a certificate for
    #[clap(long)]
    domain: Option<String>,
    /// Port to bind to
    #[clap(long, default_value = "8000")]
    port: u16,
    #[clap(long)]
    /// Email to request a LetsEncrypt cert with
    email: Option<String>,
    #[clap(long)]
    /// Path to a certificate file. Required if `wss` is given.
    cert: Option<PathBuf>,
    #[clap(long)]
    /// Path to a private key file. Required if `wss` is given.
    private_key: Option<PathBuf>,
    /// Use TLS
    #[clap(long)]
    wss: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts: Opts = Opts::parse();
    fmt::init();

    let acceptor = if opts.wss {
        if let (Some(cert), Some(private_key), Some(email), Some(domain)) =
            (opts.cert, opts.private_key, opts.email, opts.domain)
        {
            if !cert.is_file() {
                info!("Certificate doesn't exist, requesting one");
                get_cert(domain, email, &cert, &private_key).await?;
            }

            let mut config = ServerConfig::new(NoClientAuth::new());
            config.set_single_cert(
                certs(&mut BufReader::new(File::open(&cert)?)).unwrap(),
                pkcs8_private_keys(&mut BufReader::new(File::open(private_key)?))
                    .unwrap()
                    .remove(0),
            )?;
            let acceptor = TlsAcceptor::from(Arc::new(config));
            Some(acceptor)
        } else {
            anyhow::bail!("Please provide `cert,` `private_key`, `domain`, and `email` options");
        }
    } else {
        None
    };

    let endpoint = format!("{}:{}", opts.interface, opts.port);
    let listener = TcpListener::bind(&endpoint).await?;
    println!("Listening on {}", endpoint);
    let clients = Arc::new(Mutex::new(HashMap::new()));

    while let Ok((stream, peer_addr)) = listener.accept().await {
        info!(?peer_addr, "Inbound connection");
        let c = clients.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(c, stream, acceptor).await {
                println!("Error handling inbound stream: {:#}", e);
            }
        });
    }

    Ok(())
}
