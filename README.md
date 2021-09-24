# libp2p-webrtc

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](https://github.com/wngr/async-datachannel)
[![Cargo](https://img.shields.io/crates/v/libp2p-webrtc.svg)](https://crates.io/crates/libp2p-webrtc)
[![Documentation](https://docs.rs/libp2p-webrtc/badge.svg)](https://docs.rs/libp2p-webrtc)

**This currently depends on a fork of rust-libp2p, pending
https://github.com/libp2p/rust-libp2p/pull/2245**

WebRTC transport for rust-libp2p for both native and WebAssembly (browser). For
initiating a connection between two peers, both of them need to connect to the
same WebSocket signaling server (via a call to `listen_on`). Either one can dial
the other peer by using the `p2p-webrtc-star` protocol. Note, that this crate is
currently not interoperable with the respective js-libp2p and go-libp2p
implementations.

Additional, a signaling server implementation is provided within the crate.

## Quickstart

To compile this crate, you need `gcc g++ libssl-dev make cmake clang`.

```rust
let base_transport = WebRtcTransport::new(peer_id, vec!["stun:stun.l.google.com:19302"]);
let transport = base_transport.upgrade()...

..

swarm
    .listen_on(
        "/ip4/127.0.0.1/tcp/8001/ws/p2p-webrtc-star"
            .parse()
            .unwrap(),
    )
    .unwrap();
```

## License

Licensed under either of

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

#### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
