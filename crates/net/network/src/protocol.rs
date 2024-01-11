//! Support for handling additional RLPx-based application-level protocols.
//!
//! See also <https://github.com/ethereum/devp2p/blob/master/README.md>

use derive_more::Deref;
use futures::{Sink, Stream};
use reth_eth_wire::{
    capability::{SharedCapabilities, SharedCapability},
    multiplex::ProtocolStream,
    protocol::Protocol,
    CanDisconnect,
};
use reth_network_api::Direction;
use reth_primitives::bytes::{Bytes, BytesMut};
use reth_rpc_types::PeerId;
use std::{error, fmt, io, net::SocketAddr, pin::Pin};

/// A trait that allows to offer additional RLPx-based application-level protocols when establishing
/// a peer-to-peer connection.
pub trait ProtocolHandler: fmt::Debug + Send + Sync + 'static {
    /// The type responsible for negotiating the protocol with the remote.
    type ConnectionHandler: ConnectionHandler;

    /// Invoked when a new incoming connection from the remote is requested
    ///
    /// If protocols for this outgoing should be announced to the remote, return a connection
    /// handler.
    fn on_incoming(&self, socket_addr: SocketAddr) -> Option<Box<Self::ConnectionHandler>>;

    /// Invoked when a new outgoing connection to the remote is requested.
    ///
    /// If protocols for this outgoing should be announced to the remote, return a connection
    /// handler.
    fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> Option<Box<Self::ConnectionHandler>>;
}

/// A trait that allows to authenticate a protocol after the RLPx connection was established.
pub trait ConnectionHandler: Send + Sync + 'static {
    /// The connection that yields messages to send to the remote.
    ///
    /// The connection will be closed when this stream resolves.
    type Connection: Connection;

    /// The connection that transfers messages to and from the remote.
    ///
    /// The connection will be closed when this stream resolves.
    type P2PConnection: P2PConnection;

    /// Returns the protocol to announce when the p2p connection will be established.
    ///
    /// This will be used to negotiate capability message id offsets with the remote peer.
    fn protocol(&self) -> Protocol;

    /// Invoked when the RLPx connection has been established by the peer does not share the
    /// protocol.
    fn on_unsupported_by_peer(
        self,
        supported: &SharedCapabilities,
        direction: Direction,
        peer_id: PeerId,
    ) -> OnNotSupported;

    /// Invoked when the RLPx connection was established.
    ///
    /// The returned future should resolve when the connection should disconnect.
    fn into_connection(
        self,
        direction: Direction,
        peer_id: PeerId,
        conn: Self::P2PConnection,
    ) -> Option<Pin<Box<Self::Connection>>>;
}

/// What to do when a protocol is not supported by the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnNotSupported {
    /// Proceed with the connection and ignore the protocol.
    #[default]
    KeepAlive,
    /// Disconnect the connection.
    Disconnect,
}

/// An established rlpx sub protocol connection as returned by [`ConnectionHandler`].
pub trait Connection<
    StreamedType = dyn fmt::Debug,
    SunkType = Box<dyn fmt::Debug>,
    E = dyn error::Error,
>: Stream<Item = StreamedType> + Sink<SunkType, Error = E> + Send + 'static where
    StreamedType: fmt::Debug + ?Sized,
    SunkType: fmt::Debug + Sized,
    E: error::Error + ?Sized,
{
}

impl<T, StreamedType, SunkType, E> Connection<StreamedType, SunkType, E> for T
where
    T: Stream<Item = StreamedType> + Sink<SunkType, Error = E> + Send + 'static,
    StreamedType: fmt::Debug + ?Sized,
    SunkType: fmt::Debug + Sized,
    E: error::Error + ?Sized,
{
}

/// An established connection to the p2p connection. Passed to [`ConnectionHandler`] to
/// establish a rlpx subprotocol [`Connection`].
pub trait P2PConnection<E = io::Error>:
    Connection<BytesMut, Bytes, E> + CanDisconnect<Bytes> + ProxyProtocol + Send + 'static
where
    E: error::Error,
{
}

impl<T, E> P2PConnection<E> for T
where
    T: Connection<BytesMut, Bytes, E>
        + CanDisconnect<Bytes>
        + ProxyProtocol
        + Send
        + ?Sized
        + 'static,
    E: error::Error,
{
}
/// Act as intermediary between p2p connection and protocol connection.
pub trait ProxyProtocol {
    /// Shared capability assigned to proxy.
    fn shared_capability(&self) -> &SharedCapability;

    /// Returns the message with masked message ID.
    ///
    /// Mask the message ID of outgoing messages relative to suffix used for capability message
    /// IDs. [`reth_eth_wire::P2PStream`] further masks the message ID relative to the reserved
    /// p2p prefix. (todo: mask ID completely at this layer or sink BytesMut)
    fn relative_mask_msg_id(&self, msg: BytesMut) -> Bytes;
}

impl ProxyProtocol for ProtocolStream {
    fn shared_capability(&self) -> &SharedCapability {
        self.cap()
    }

    fn relative_mask_msg_id(&self, msg: BytesMut) -> Bytes {
        self.mask_msg_id(msg)
    }
}

/// Convenience type setting associated type for [`ProtocolHandler`].
pub type DynProtocolHandler = dyn ProtocolHandler<ConnectionHandler = DynConnectionHandler>;

/// Convenience type setting associated types for [`ConnectionHandler`].
pub type DynConnectionHandler =
    dyn ConnectionHandler<Connection = dyn Connection, P2PConnection = ProtocolStream>;

/// A wrapper type for a RLPx sub-protocol.
#[derive(Debug, Deref)]
pub struct RlpxSubProtocol(Box<DynProtocolHandler>);

/// A helper trait to convert a [ProtocolHandler] into a dynamic type
pub trait IntoRlpxSubProtocol {
    /// Converts the type into a [RlpxSubProtocol].
    fn into_rlpx_sub_protocol(self) -> RlpxSubProtocol;
}

impl<T> IntoRlpxSubProtocol for T
where
    T: ProtocolHandler<ConnectionHandler = DynConnectionHandler> + Send + Sync + 'static,
{
    fn into_rlpx_sub_protocol(self) -> RlpxSubProtocol {
        RlpxSubProtocol(Box::new(self))
    }
}

impl IntoRlpxSubProtocol for RlpxSubProtocol {
    fn into_rlpx_sub_protocol(self) -> RlpxSubProtocol {
        self
    }
}

/// Additional RLPx-based sub-protocols.
#[derive(Debug, Default, Deref)]
pub struct RlpxSubProtocols {
    /// All extra protocols
    protocols: Vec<RlpxSubProtocol>,
}

impl RlpxSubProtocols {
    /// Adds a new protocol.
    pub fn push(&mut self, protocol: impl IntoRlpxSubProtocol) {
        self.protocols.push(protocol.into_rlpx_sub_protocol());
    }

    /// Returns all additional protocol handlers that should be announced to the remote during the
    /// Rlpx handshake on an incoming connection.
    pub(crate) fn on_incoming(&self, socket_addr: SocketAddr) -> RlpxSubProtocolHandlers {
        RlpxSubProtocolHandlers(
            self.protocols
                .iter()
                .filter_map(|protocol| protocol.0.on_incoming(socket_addr))
                .collect(),
        )
    }

    /// Returns all additional protocol handlers that should be announced to the remote during the
    /// Rlpx handshake on an outgoing connection.
    pub(crate) fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> RlpxSubProtocolHandlers {
        RlpxSubProtocolHandlers(
            self.protocols
                .iter()
                .filter_map(|protocol| protocol.0.on_outgoing(socket_addr, peer_id))
                .collect(),
        )
    }
}

/// A set of additional RLPx-based sub-protocol connection handlers.
#[derive(Default)]
pub(crate) struct RlpxSubProtocolHandlers(Vec<Box<dyn DynConnectionHandler>>);

impl RlpxSubProtocolHandlers {
    /// Returns all handlers.
    pub(crate) fn into_iter(self) -> impl Iterator<Item = Box<dyn DynConnectionHandler>> {
        self.0.into_iter()
    }
}

impl Deref for RlpxSubProtocolHandlers {
    type Target = Vec<Box<dyn DynConnectionHandler>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RlpxSubProtocolHandlers {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
