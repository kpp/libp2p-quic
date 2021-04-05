use crate::{QuicError, QuicMuxer};
use bytes::{Buf, BufMut, BytesMut};
use libp2p::core::StreamMuxer;
use libp2p::PeerId;
use quinn_proto::crypto::{
    ClientConfig, CryptoError, ExportKeyingMaterialError, HeaderKey, KeyPair, Keys, PacketKey,
    ServerConfig, Session,
};
use quinn_proto::transport_parameters::TransportParameters;
use quinn_proto::{ConnectError, ConnectionId, Side, TransportError, TransportErrorCode};
use ring::aead;
use std::future::Future;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

pub struct NoiseUpgrade(Option<QuicMuxer>);

impl NoiseUpgrade {
    pub fn new(muxer: QuicMuxer) -> Self {
        Self(Some(muxer))
    }
}

impl Future for NoiseUpgrade {
    type Output = Result<(PeerId, QuicMuxer), QuicError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let muxer = self.0.as_ref().unwrap();
        match muxer.poll_event(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Err(QuicError::UpgradeError)),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err.into())),
            Poll::Pending => {
                if !muxer.is_handshaking() {
                    if let Some(peer_id) = muxer.peer_id() {
                        tracing::trace!("noise upgrade complete");
                        return Poll::Ready(Ok((peer_id, self.0.take().unwrap())));
                    }
                }
                Poll::Pending
            }
        }
    }
}

pub type IdentityKeypair = libp2p::core::identity::Keypair;

#[derive(Clone)]
pub struct NoiseConfig {
    pub(crate) params: snow::params::NoiseParams,
    pub(crate) keypair: IdentityKeypair,
    pub(crate) prologue: Vec<u8>,
}

impl std::fmt::Debug for NoiseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("NoiseConfig")
            .field("params", &self.params)
            .field("keypair", &self.keypair.public().into_peer_id().to_string())
            .field("prologue", &self.prologue)
            .finish()
    }
}

impl NoiseConfig {
    fn default() -> Self {
        Self {
            params: "Noise_XX_25519_AESGCM_SHA256".parse().unwrap(),
            keypair: IdentityKeypair::generate_ed25519(),
            prologue: Default::default(),
        }
    }
}

impl ClientConfig<NoiseSession> for NoiseConfig {
    fn new() -> Self {
        Self::default()
    }

    fn start_session(
        &self,
        _: &str,
        params: &TransportParameters,
    ) -> Result<NoiseSession, ConnectError> {
        Ok(self.start_session(Side::Client, params))
    }
}

impl ServerConfig<NoiseSession> for NoiseConfig {
    fn new() -> Self {
        Self::default()
    }

    fn start_session(&self, params: &TransportParameters) -> NoiseSession {
        self.start_session(Side::Server, params)
    }
}

impl NoiseConfig {
    fn start_session(&self, side: Side, params: &TransportParameters) -> NoiseSession {
        let builder = snow::Builder::new(self.params.clone()).prologue(&self.prologue);
        let x25519 = builder.generate_keypair().unwrap();
        let builder = builder.local_private_key(&x25519.private);
        let signed_x25519_key = self.keypair.sign(&x25519.public).unwrap();
        let noise = if side == Side::Client {
            builder.build_initiator().unwrap()
        } else {
            builder.build_responder().unwrap()
        };
        NoiseSession {
            state: Some(HandshakeState {
                side,
                state: State::ClientInit,
                noise,
                transport_parameters: *params,
                identity: Identity {
                    public_key: self.keypair.public().into_protobuf_encoding(),
                    signed_x25519_key,
                },
            }),
            remote_transport_parameters: None,
            remote_public_key: None,
        }
    }
}

struct Identity {
    public_key: Vec<u8>,
    signed_x25519_key: Vec<u8>,
}

impl Identity {
    pub fn write<W: BufMut>(&self, w: &mut W) {
        w.put_u16(self.public_key.len() as u16);
        w.put_slice(&self.public_key);
        w.put_u16(self.signed_x25519_key.len() as u16);
        w.put_slice(&self.signed_x25519_key);
    }

    pub fn read<R: Buf>(
        r: &mut R,
        remote_static: &[u8],
    ) -> anyhow::Result<libp2p::core::identity::PublicKey> {
        anyhow::ensure!(
            r.remaining() > 2,
            "identity too small: less than 2 remaining"
        );
        let len = r.get_u16() as usize;
        anyhow::ensure!(
            r.remaining() > len,
            "identity too small: less than {} remaining",
            len
        );
        let public_key =
            libp2p::core::identity::PublicKey::from_protobuf_encoding(r.take(len).chunk())?;
        anyhow::ensure!(
            r.remaining() > 2,
            "identity too small: less than 2 remaining"
        );
        r.advance(len);
        let len = r.get_u16() as usize;
        anyhow::ensure!(
            r.remaining() >= len,
            "identity too small: less than {} remaining",
            len
        );
        let valid = public_key.verify(remote_static, r.take(len).chunk());
        anyhow::ensure!(valid, "invalid signature");
        r.advance(len);
        Ok(public_key)
    }
}

struct HandshakeState {
    side: Side,
    state: State,
    noise: snow::HandshakeState,
    transport_parameters: TransportParameters,
    identity: Identity,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum State {
    ClientInit,
    ServerInit,
    HandshakeKeysReady,
    ServerHandshake,
    ClientHandshake,
    HandshakeComplete,
}

pub struct NoiseSession {
    state: Option<HandshakeState>,
    remote_transport_parameters: Option<TransportParameters>,
    remote_public_key: Option<libp2p::core::identity::PublicKey>,
}

impl Session for NoiseSession {
    type HandshakeData = libp2p::core::identity::PublicKey;
    type Identity = PeerId;
    type ClientConfig = NoiseConfig;
    type ServerConfig = NoiseConfig;
    type HmacKey = ring::hmac::Key;
    type HandshakeTokenKey = ring::hkdf::Prk;
    type HeaderKey = PlaintextHeaderKey;
    type PacketKey = NoisePacketKey;

    fn initial_keys(_: &ConnectionId, _: Side) -> Keys<Self> {
        Keys {
            header: KeyPair {
                local: PlaintextHeaderKey,
                remote: PlaintextHeaderKey,
            },
            packet: KeyPair {
                local: NoisePacketKey::Initial,
                remote: NoisePacketKey::Initial,
            },
        }
    }

    fn next_1rtt_keys(&mut self) -> Option<KeyPair<Self::PacketKey>> {
        // TODO!!!
        Some(KeyPair {
            local: NoisePacketKey::NextKey,
            remote: NoisePacketKey::NextKey,
        })
    }

    fn read_handshake(&mut self, handshake: &[u8]) -> Result<bool, TransportError> {
        let mut payload;
        let state = self.state.as_mut().ok_or_else(|| TransportError {
            code: TransportErrorCode::CONNECTION_REFUSED,
            frame: None,
            reason: "unexpected crypto frame".to_string(),
        })?;
        let mut cursor = if state.side == Side::Client && state.state == State::ServerHandshake {
            Cursor::new(handshake)
        } else {
            payload = vec![0; handshake.len()];
            let size = state
                .noise
                .read_message(handshake, &mut payload)
                .map_err(|err| TransportError {
                    code: TransportErrorCode::CONNECTION_REFUSED,
                    frame: None,
                    reason: err.to_string(),
                })?;
            payload.truncate(size);
            Cursor::new(&payload[..])
        };
        match (state.side, state.state) {
            (Side::Server, State::ClientInit) => {
                self.remote_transport_parameters =
                    Some(TransportParameters::read(Side::Server, &mut cursor)?);
                state.state = State::ServerInit;
                Ok(false)
            }
            (Side::Client, State::ServerInit) => {
                self.remote_transport_parameters =
                    Some(TransportParameters::read(Side::Client, &mut cursor)?);
                state.state = State::HandshakeKeysReady;
                Ok(false)
            }
            (Side::Client, State::ServerHandshake) => {
                let remote_static = state.noise.get_remote_static().unwrap();
                let remote_public =
                    Identity::read(&mut cursor, remote_static).map_err(|err| TransportError {
                        code: TransportErrorCode::CONNECTION_REFUSED,
                        frame: None,
                        reason: err.to_string(),
                    })?;
                self.remote_public_key = Some(remote_public);
                state.state = State::ClientHandshake;
                Ok(true)
            }
            (Side::Server, State::ClientHandshake) => {
                let remote_static = state.noise.get_remote_static().unwrap();
                let remote_public =
                    Identity::read(&mut cursor, remote_static).map_err(|err| TransportError {
                        code: TransportErrorCode::CONNECTION_REFUSED,
                        frame: None,
                        reason: err.to_string(),
                    })?;
                self.remote_public_key = Some(remote_public);
                state.state = State::HandshakeComplete;
                Ok(true)
            }
            _ => Err(TransportError {
                code: TransportErrorCode::CONNECTION_REFUSED,
                frame: None,
                reason: "unexpected crypto frame".to_string(),
            }),
        }
    }

    fn write_handshake(&mut self, handshake: &mut Vec<u8>) -> Option<Keys<Self>> {
        let state = self.state.as_mut()?;
        let mut payload = vec![];
        match (state.side, state.state) {
            (Side::Client, State::ClientInit) => {
                state.transport_parameters.write(&mut payload);
                state.state = State::ServerInit;
            }
            (Side::Server, State::ClientInit) => {
                return None;
            }
            (Side::Client, State::ServerInit) => {
                return None;
            }
            (Side::Server, State::ServerInit) => {
                state.transport_parameters.write(&mut payload);
                state.state = State::ServerHandshake;
            }
            (Side::Client, State::HandshakeKeysReady) => {
                state.state = State::ServerHandshake;
            }
            (Side::Server, State::HandshakeKeysReady) => {
                unreachable!()
            }
            (Side::Client, State::ServerHandshake) => {
                return None;
            }
            (Side::Server, State::ServerHandshake) => {
                state.identity.write(&mut payload);
                state.state = State::ClientHandshake;
            }
            (Side::Client, State::ClientHandshake) => {
                state.identity.write(&mut payload);
                state.state = State::HandshakeComplete;
            }
            (Side::Server, State::ClientHandshake) => {
                return None;
            }
            (_, State::HandshakeComplete) => {}
        };
        if !payload.is_empty() {
            if state.side == Side::Server && state.state == State::ClientHandshake {
                handshake.extend_from_slice(&payload);
            } else {
                handshake.resize(1200, 0);
                let size = state.noise.write_message(&payload, handshake).unwrap();
                handshake.truncate(size);
            }
        }
        if state.state == State::ServerHandshake {
            let hash = state.noise.get_handshake_hash().to_vec();
            Some(Keys {
                header: KeyPair {
                    local: PlaintextHeaderKey,
                    remote: PlaintextHeaderKey,
                },
                packet: KeyPair {
                    local: NoisePacketKey::Handshake(hash.clone()),
                    remote: NoisePacketKey::Handshake(hash),
                },
            })
        } else if state.state == State::HandshakeComplete {
            tracing::trace!("handshake complete");
            let state = self.state.take().unwrap();
            let key = Arc::new(state.noise.into_stateless_transport_mode().unwrap());
            Some(Keys {
                header: KeyPair {
                    local: PlaintextHeaderKey,
                    remote: PlaintextHeaderKey,
                },
                packet: KeyPair {
                    local: NoisePacketKey::Transport(key.clone()),
                    remote: NoisePacketKey::Transport(key),
                },
            })
        } else {
            None
        }
    }

    fn is_handshaking(&self) -> bool {
        self.state.is_some()
    }

    fn peer_identity(&self) -> Option<Self::Identity> {
        let remote_public_key = self.remote_public_key.as_ref()?;
        Some(remote_public_key.clone().into_peer_id())
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        Ok(self.remote_transport_parameters)
    }

    fn handshake_data(&self) -> Option<Self::HandshakeData> {
        self.remote_public_key.clone()
    }

    fn export_keying_material(
        &self,
        _: &mut [u8],
        _: &[u8],
        _: &[u8],
    ) -> Result<(), ExportKeyingMaterialError> {
        // TODO optional
        Err(ExportKeyingMaterialError)
    }

    fn early_crypto(&self) -> Option<(Self::HeaderKey, Self::PacketKey)> {
        // 0-rtt is unsupported
        None
    }

    fn early_data_accepted(&self) -> Option<bool> {
        // 0-rtt is unsupported
        None
    }

    // TODO: add default implementation to quinn
    fn retry_tag(orig_dst_cid: &ConnectionId, packet: &[u8]) -> [u8; 16] {
        let mut pseudo_packet = Vec::with_capacity(packet.len() + orig_dst_cid.len() + 1);
        pseudo_packet.push(orig_dst_cid.len() as u8);
        pseudo_packet.extend_from_slice(orig_dst_cid);
        pseudo_packet.extend_from_slice(packet);

        let nonce = aead::Nonce::assume_unique_for_key(RETRY_INTEGRITY_NONCE);
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_128_GCM, &RETRY_INTEGRITY_KEY).unwrap(),
        );

        let tag = key
            .seal_in_place_separate_tag(nonce, aead::Aad::from(pseudo_packet), &mut [])
            .unwrap();
        let mut result = [0; 16];
        result.copy_from_slice(tag.as_ref());
        result
    }

    // TODO: add default implementation to quinn
    fn is_valid_retry(orig_dst_cid: &ConnectionId, header: &[u8], payload: &[u8]) -> bool {
        let tag_start = match payload.len().checked_sub(16) {
            Some(x) => x,
            None => return false,
        };

        let mut pseudo_packet =
            Vec::with_capacity(header.len() + payload.len() + orig_dst_cid.len() + 1);
        pseudo_packet.push(orig_dst_cid.len() as u8);
        pseudo_packet.extend_from_slice(orig_dst_cid);
        pseudo_packet.extend_from_slice(header);
        let tag_start = tag_start + pseudo_packet.len();
        pseudo_packet.extend_from_slice(payload);

        let nonce = aead::Nonce::assume_unique_for_key(RETRY_INTEGRITY_NONCE);
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_128_GCM, &RETRY_INTEGRITY_KEY).unwrap(),
        );

        let (aad, tag) = pseudo_packet.split_at_mut(tag_start);
        key.open_in_place(nonce, aead::Aad::from(aad), tag).is_ok()
    }
}

const RETRY_INTEGRITY_KEY: [u8; 16] = [
    0xcc, 0xce, 0x18, 0x7e, 0xd0, 0x9a, 0x09, 0xd0, 0x57, 0x28, 0x15, 0x5a, 0x6c, 0xb9, 0x6b, 0xe1,
];
const RETRY_INTEGRITY_NONCE: [u8; 12] = [
    0xe5, 0x49, 0x30, 0xf9, 0x7f, 0x21, 0x36, 0xf0, 0x53, 0x0a, 0x8c, 0x1c,
];

pub enum NoisePacketKey {
    /// Initial key for first packet. We send the first packet in plain text.
    Initial,
    /// Handshake key used during the handshake.
    Handshake(Vec<u8>),
    /// After the handshake is complete the noise state is used to encrypt packets.
    Transport(Arc<snow::StatelessTransportState>),
    /// When the key is exhausted due to integrity or confidentiality limits, the key
    /// is rotated.
    NextKey,
}

impl PacketKey for NoisePacketKey {
    fn encrypt(&self, packet: u64, buf: &mut [u8], header_len: usize) {
        match self {
            Self::Initial => {}
            Self::Handshake(_hash) => {
                // TODO: encrypt with handshake hash
            }
            Self::Transport(state) => {
                // TODO: provide the header as assiciated data
                // TODO: mutate the buffer in place
                let (_header, payload) = buf.split_at_mut(header_len);
                let mut buffer = vec![0; payload.len()];
                let (content, _tag) = payload.split_at_mut(payload.len() - self.tag_len());
                state.write_message(packet, content, &mut buffer).unwrap();
                payload.copy_from_slice(&buffer);
            }
            Self::NextKey => panic!("key rotation is not implemented"),
        }
    }

    fn decrypt(
        &self,
        packet: u64,
        _header: &[u8],
        payload: &mut BytesMut,
    ) -> Result<(), CryptoError> {
        match self {
            Self::Initial => {}
            Self::Handshake(_hash) => {
                // TODO: decrypt with handshake hash
            }
            Self::Transport(state) => {
                // TODO: provide the header as assiciated data
                // TODO: mutate the buffer in place
                if payload.len() < self.tag_len() {
                    return Err(CryptoError);
                }
                let mut buffer = vec![0; payload.len() - self.tag_len()];
                state
                    .read_message(packet, payload, &mut buffer)
                    .map_err(|_| CryptoError)?;
                payload.truncate(buffer.len());
                payload.copy_from_slice(&buffer);
            }
            Self::NextKey => panic!("key rotation is not implemented"),
        };
        Ok(())
    }

    fn tag_len(&self) -> usize {
        16
    }

    fn confidentiality_limit(&self) -> u64 {
        // TODO: noise spec doesn't mention anything specific and assumes `u64::MAX`.
        u64::MAX
    }

    fn integrity_limit(&self) -> u64 {
        // TODO: noise spec doesn't mention anything specific and assumes `u64::MAX`.
        u64::MAX
    }
}

pub struct PlaintextHeaderKey;

impl HeaderKey for PlaintextHeaderKey {
    fn decrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {}

    fn encrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {}

    fn sample_size(&self) -> usize {
        0
    }
}
