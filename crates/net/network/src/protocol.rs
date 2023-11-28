//! Support for handling additional RLPx-based application-level protocols.
//!
//! See also <https://github.com/ethereum/devp2p/blob/master/README.md>

use derive_more::{Deref, DerefMut};
use futures::{Sink, Stream};
use reth_eth_wire::{capability::SharedCapabilities, protocol::Protocol, StreamClone};
use reth_network_api::Direction;
use reth_rpc_types::PeerId;
use std::{error::Error, fmt, net::SocketAddr, pin::Pin, sync::Arc};

/// A connection to the p2p connection, that doesn't own the p2p connection. Dependent on
/// [`crate::PeerConnection`] to make progress.
pub type WeakPeerConnection = StreamClone;

/// A trait that allows to offer additional RLPx-based application-level protocols when establishing
/// a peer-to-peer connection.
pub trait ProtocolHandler: fmt::Debug + Send + Sync + 'static {
    /// The type responsible for negotiating the protocol with the remote.
    type ConnectionHandler: ConnectionHandler;

    /// Returns the protocol to announce via [`crate::HelloMessageWithProtocols`] when the P2P  
    /// connection will be established.
    ///
    /// Then it is negotiated with the remote peer wether this RLPx sub-protocol connection wil be
    /// opened or not.
    fn protocol(&self) -> Protocol;

    /// Invoked when a new incoming connection from the remote is requested
    ///
    /// If protocols for this outgoing should be announced to the remote, return a connection
    /// handler.
    fn on_incoming(&self, socket_addr: SocketAddr) -> Option<Self::ConnectionHandler>;

    /// Invoked when a new outgoing connection to the remote is requested.
    ///
    /// If protocols for this outgoing should be announced to the remote, return a connection
    /// handler.
    fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> Option<Self::ConnectionHandler>;
}

/// Stream messages to and from app <> capability stream.
pub trait StreamInAppMessages:
    Stream<Item = dyn fmt::Debug> // e.g. stream ActiveSessionMessage
    + Sink<Arc<dyn fmt::Debug>, Error = dyn Error> // e.g. SessionCommand
    + Send
    + fmt::Debug
    + 'static
{
}

/// A trait that allows to authenticate a protocol after the RLPx connection was established.
pub trait ConnectionHandler: Send + Sync + fmt::Debug + 'static {
    /// The connection that yields messages to send to the remote and processes messages from the
    /// remote.
    ///
    /// The connection will be closed when this stream resolves.
    type Connection: StreamInAppMessages;

    /// Invoked when the RLPx connection has been established by the peer does not share the
    /// protocol.
    fn on_unsupported_by_peer(
        self: Box<Self>,
        supported: &SharedCapabilities,
        direction: Direction,
        peer_id: PeerId,
    ) -> OnNotSupported;

    /// Invoked when the RLPx connection was established.
    ///
    /// The returned future should resolve when the connection should disconnect.
    fn into_connection(
        self: Box<Self>,
        direction: Direction,
        peer_id: PeerId,
        p2p_conn: WeakPeerConnection,
    ) -> Self::Connection;
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

/// A wrapper type for a RLPx sub-protocol.
#[derive(Debug, Deref, DerefMut)]
pub struct RlpxSubProtocol(Box<dyn DynProtocolHandler>);

/// A helper trait to convert a [ProtocolHandler] into a dynamic type
pub trait IntoRlpxSubProtocol {
    /// Converts the type into a [RlpxSubProtocol].
    fn into_rlpx_sub_protocol(self) -> RlpxSubProtocol;
}

impl<T> IntoRlpxSubProtocol for T
where
    T: ProtocolHandler + Send + Sync + 'static,
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
#[derive(Debug, Default, Deref, Clone)]
pub struct RlpxSubProtocols {
    /// All extra protocols
    protocols: Vec<Arc<RlpxSubProtocol>>,
}

impl RlpxSubProtocols {
    /// Adds a new protocol.
    pub fn push(&mut self, protocol: impl IntoRlpxSubProtocol) {
        self.protocols.push(Arc::new(protocol.into_rlpx_sub_protocol()));
    }
}

/// Wrapper around [`ProtocolHandler`] that casts its return types as trait objects.
pub trait DynProtocolHandler: fmt::Debug + Send + Sync + 'static {
    /// Same as method for [`ProtocolHandler`].
    fn protocol(&self) -> Protocol;

    /// Returns a trait object that implements [`ConnectionHandler`].
    fn on_incoming(&self, socket_addr: SocketAddr) -> Option<Box<dyn DynConnectionHandler>>;

    /// Returns a trait object that implements [`ConnectionHandler`].
    fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> Option<Box<dyn DynConnectionHandler>>;
}

impl<T> DynProtocolHandler for T
where
    T: ProtocolHandler,
{
    fn protocol(&self) -> Protocol {
        T::protocol(self)
    }

    fn on_incoming(&self, socket_addr: SocketAddr) -> Option<Box<dyn DynConnectionHandler>> {
        T::on_incoming(self, socket_addr)
            .map(|handler| Box::new(handler) as Box<dyn DynConnectionHandler>)
    }

    fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> Option<Box<dyn DynConnectionHandler>> {
        T::on_outgoing(self, socket_addr, peer_id)
            .map(|handler| Box::new(handler) as Box<dyn DynConnectionHandler>)
    }
}

/// Wrapper trait for internal ease of use.
pub trait DynConnectionHandler: Send + Sync + fmt::Debug + 'static {
    /// See [`ConnectionHandler`].
    fn on_unsupported_by_peer(
        self: Box<Self>,
        supported: &SharedCapabilities,
        direction: Direction,
        peer_id: PeerId,
    ) -> OnNotSupported;

    /// Returns a trait object that implements
    fn into_connection(
        self: Box<Self>,
        direction: Direction,
        peer_id: PeerId,
        p2p_conn: WeakPeerConnection,
    ) -> Pin<Box<dyn StreamInAppMessages>>;
}

impl<T> DynConnectionHandler for T
where
    T: ConnectionHandler,
{
    fn on_unsupported_by_peer(
        self: Box<Self>,
        supported: &SharedCapabilities,
        direction: Direction,
        peer_id: PeerId,
    ) -> OnNotSupported {
        T::on_unsupported_by_peer(self, supported, direction, peer_id)
    }

    fn into_connection(
        self: Box<Self>,
        direction: Direction,
        peer_id: PeerId,
        p2p_conn: WeakPeerConnection,
    ) -> Pin<Box<dyn StreamInAppMessages>> {
        Box::pin(T::into_connection(self, direction, peer_id, p2p_conn))
    }
}
