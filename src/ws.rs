use std::task::Poll;

use async_tungstenite::{
    tokio::{connect_async, ConnectStream},
    tungstenite::Message,
    WebSocketStream,
};
use libp2p::futures::{Sink, Stream};
use pin_project::pin_project;

#[pin_project(project = EnumProj)]
enum InnerStream {
    #[cfg(not(target_arch = "wasm32"))]
    Native(#[pin] WebSocketStream<ConnectStream>),
    #[cfg(target_arch = "wasm32")]
    Wasm(#[pin] ws_stream_wasm::WsStream),
}

#[pin_project]
pub(crate) struct CombinedStream {
    #[pin]
    inner: InnerStream,
}

impl CombinedStream {
    pub(crate) async fn connect(uri: &str) -> anyhow::Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        let inner = InnerStream::Native(connect_async(uri).await?.0);
        #[cfg(target_arch = "wasm32")]
        let inner = InnerStream::Wasm(ws_stream_wasm::WsMeta::connect(uri, None).await?.1);
        Ok(Self { inner })
    }
}

impl Stream for CombinedStream {
    type Item = anyhow::Result<Message>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.project().inner.project() {
            #[cfg(not(target_arch = "wasm32"))]
            EnumProj::Native(s) => s.poll_next(cx).map_err(Into::into),
            #[cfg(target_arch = "wasm32")]
            EnumProj::Wasm(s) => match s.poll_next(cx) {
                Poll::Ready(Some(x)) => match x {
                    ws_stream_wasm::WsMessage::Text(t) => Poll::Ready(Some(Ok(Message::Text(t)))),
                    ws_stream_wasm::WsMessage::Binary(t) => {
                        Poll::Ready(Some(Ok(Message::Binary(t))))
                    }
                },
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl Sink<Message> for CombinedStream {
    type Error = anyhow::Error;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.project().inner.project() {
            #[cfg(not(target_arch = "wasm32"))]
            EnumProj::Native(s) => s.poll_ready(cx).map_err(Into::into),
            #[cfg(target_arch = "wasm32")]
            EnumProj::Wasm(s) => s.poll_ready(cx).map_err(Into::into),
        }
    }

    fn start_send(self: std::pin::Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        match self.project().inner.project() {
            #[cfg(not(target_arch = "wasm32"))]
            EnumProj::Native(s) => s.start_send(item).map_err(Into::into),
            #[cfg(target_arch = "wasm32")]
            EnumProj::Wasm(s) => {
                if let Some(msg) = match item {
                    Message::Text(t) => Some(ws_stream_wasm::WsMessage::Text(t)),
                    Message::Binary(b) => Some(ws_stream_wasm::WsMessage::Binary(b)),
                    _ => None,
                } {
                    s.start_send(msg).map_err(Into::into)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.project().inner.project() {
            #[cfg(not(target_arch = "wasm32"))]
            EnumProj::Native(s) => s.poll_flush(cx).map_err(Into::into),
            #[cfg(target_arch = "wasm32")]
            EnumProj::Wasm(s) => s.poll_flush(cx).map_err(Into::into),
        }
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.project().inner.project() {
            #[cfg(not(target_arch = "wasm32"))]
            EnumProj::Native(s) => s.poll_close(cx).map_err(Into::into),
            #[cfg(target_arch = "wasm32")]
            EnumProj::Wasm(s) => s.poll_close(cx).map_err(Into::into),
        }
    }
}
