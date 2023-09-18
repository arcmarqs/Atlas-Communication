use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

use atlas_common::peer_addr::PeerAddr;
use dashmap::DashMap;
use log::{debug, error, info, warn};

use atlas_common::channel::{ChannelMixedRx, ChannelMixedTx, new_bounded_mixed, new_oneshot_channel, OneShotRx};
use atlas_common::error::*;
use atlas_common::node_id::{NodeId, NodeType};
use atlas_common::socket::SecureReadHalf;
use atlas_common::socket::SecureWriteHalf;

use crate::client_pooling::{ConnectedPeer, PeerIncomingRqHandling};
use crate::message::{StoredMessage, WireMessage};
use crate::NodeConnections;
use crate::config::TcpConfig;
use crate::reconfiguration_node::{NetworkInformationProvider, ReconfigurationMessageHandler};
use crate::serialize::Serializable;
use crate::tcpip::{NodeConnectionAcceptor, TlsNodeAcceptor, TlsNodeConnector};
use crate::tcpip::connections::conn_establish::ConnectionHandler;

mod incoming;
mod outgoing;
mod conn_establish;

pub type Callback = Option<Box<dyn FnOnce(bool) -> () + Send>>;

pub type NetworkSerializedMessage = (WireMessage, Callback, Instant, bool, Instant);

/// How many slots the outgoing queue has for messages.
const TX_CONNECTION_QUEUE: usize = 1024;



/// Represents a connection between two peers
/// We can have multiple underlying tcp connections for a given connection between two peers
pub struct PeerConnection<RM, PM>
    where RM: Serializable + 'static,
          PM: Serializable + 'static, {
    // The ID of the connected node
    peer_node_id: NodeId,
    my_id: NodeId,
    //A handle to the request buffer of the peer we are connected to in the client pooling module
    client: Arc<ConnectedPeer<StoredMessage<PM::Message>>>,
    // A handle to the reconfiguration message handler
    reconf_handling: Arc<ReconfigurationMessageHandler<StoredMessage<RM::Message>>>,
    //The channel used to send serialized messages to the tasks that are meant to handle them
    tx: ChannelMixedTx<NetworkSerializedMessage>,
    // The RX handle corresponding to the tx channel above. This is so we can quickly associate new
    // TX connections to a given connection, as we just have to clone this handle
    rx: ChannelMixedRx<NetworkSerializedMessage>,
    // Counter to assign unique IDs to each of the underlying Tcp streams
    conn_id_generator: AtomicU32,
    // A map to manage the currently active connections and a cached size value to prevent
    // concurrency for simple length checks
    active_connection_count: AtomicUsize,
    active_connections: Mutex<BTreeMap<u32, ConnHandle>>,
}

#[derive(Clone)]
pub struct ConnHandle {
    id: u32,
    my_id: NodeId,
    peer_id: NodeId,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl ConnHandle {
    pub fn new(id: u32, my_id: NodeId, peer_id: NodeId) -> Self {
        Self {
            id,
            my_id,
            peer_id,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    #[inline]
    pub fn my_id(&self) -> NodeId {
        self.my_id
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

impl<RM, PM> PeerConnection<RM, PM>
    where RM: Serializable + 'static,
          PM: Serializable + 'static, {
    pub fn new_peer(
        node_id: NodeId,
        client: Arc<ConnectedPeer<StoredMessage<PM::Message>>>,
        reconf_handling: Arc<ReconfigurationMessageHandler<StoredMessage<RM::Message>>>) -> Arc<Self> {
        let (tx, rx) = new_bounded_mixed(TX_CONNECTION_QUEUE);

        Arc::new(Self {
            peer_node_id: client.client_id().clone(),
            my_id: node_id,
            client,
            reconf_handling,
            tx,
            rx,
            conn_id_generator: AtomicU32::new(0),
            active_connection_count: AtomicUsize::new(0),
            active_connections: Mutex::new(BTreeMap::new()),
        })
    }

    /// Send a message through this connection. Only valid for peer connections
    pub(crate) fn peer_message(&self, msg: WireMessage, callback: Callback, should_flush: bool, send_rq_time: Instant) -> Result<()> {
        let from = msg.header().from();
        let to = msg.header().to();

        if let Err(_) = self.tx.send((msg, callback, Instant::now(), should_flush, send_rq_time)) {
            error!("{:?} // Failed to send peer message to {:?}", from,
                to);

            return Err(Error::simple(ErrorKind::Communication));
        }

        Ok(())
    }

    async fn peer_msg_return_async(&self, to_send: NetworkSerializedMessage) -> Result<()> {
        let send = self.tx.clone();

        if let Err(_) = send.send_async(to_send).await {
            return Err(Error::simple(ErrorKind::Communication));
        }

        Ok(())
    }

    /// Initialize a new Connection Handle for a new TCP Socket
    fn init_conn_handle(&self) -> ConnHandle {
        let conn_id = self.conn_id_generator.fetch_add(1, Ordering::Relaxed);

        ConnHandle {
            id: conn_id,
            my_id: self.my_id,
            peer_id: self.peer_node_id,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get the current connection count for this connection
    pub fn connection_count(&self) -> usize {
        self.active_connection_count.load(Ordering::Relaxed)
    }

    /// Accept a TCP Stream and insert it into this connection
    pub(crate) fn insert_new_connection<NI>(self: &Arc<Self>, node_conns: Arc<PeerConnections<NI, RM, PM>>,
                                            socket: (SecureWriteHalf, SecureReadHalf), conn_limit: usize)
        where NI: NetworkInformationProvider + 'static {
        let conn_handle = self.init_conn_handle();

        let previous = {
            //Store the connection in the map
            let mut guard = self.active_connections.lock().unwrap();

            guard.insert(conn_handle.id, conn_handle.clone())
        };

        let clone = Arc::clone(self);

        if let None = previous {
            let current_connections = self.active_connection_count.fetch_add(1, Ordering::Relaxed);

            //FIXME: This is a hack to prevent a bug where all replicas attempting to connect at once meant that
            // We first stored the connection we established (since that would be established first) and then the connection
            // that the other node is trying to establish (which will also be registered first) would be disposed of by us
            // since we would think we already have a connection to that node, but we don't because that node would do the same
            if current_connections >= conn_limit * 2 {
                self.active_connection_count.fetch_sub(1, Ordering::Relaxed);

                warn!("{:?} // Already reached the max connections for the peer {:?}, disposing of connection",
                    node_conns.id(),
                    self.peer_node_id,);

                return;
            } else {
                let id = clone.client_pool_peer().client_id();

                debug!("{:?} // New connection to peer {:?} with id {:?} conn count: {}. DEBUG: {:?}",
                    node_conns.id(),
                    self.peer_node_id,
                    conn_handle.id,
                    current_connections + 1,
                    id);

                //Spawn the corresponding handlers for each side of the connection
                outgoing::spawn_outgoing_task_handler(conn_handle.clone(), node_conns.clone(), clone.clone(), socket.0);
                incoming::spawn_incoming_task_handler(conn_handle, node_conns, clone, socket.1);
            }
        } else {
            todo!("How do we handle this really?
                        This probably means it went all the way around u32 connections, really weird")
        }
    }

    /// Delete a given tcp stream from this connection
    pub(crate) fn delete_connection(&self, conn_id: u32) -> usize {
        // Remove the corresponding connection from the map
        let conn_handle = {
            // Do it inside a tiny scope to minimize the time the mutex is accessed
            let mut guard = self.active_connections.lock().unwrap();

            guard.remove(&conn_id)
        };

        let remaining_conns = if let Some(conn_handle) = conn_handle {
            let conn_count = self.active_connection_count.fetch_sub(1, Ordering::Relaxed);

            //Setting the cancelled variable to true causes all associated threads to be
            //killed (as soon as they see the warning)
            conn_handle.cancelled.store(true, Ordering::Relaxed);

            conn_count
        } else {
            self.active_connection_count.load(Ordering::Relaxed)
        };

        warn!("Connection {} with peer {:?} has been deleted",
            conn_id, self.peer_node_id);

        remaining_conns
    }

    /// Get the handle to the client pool buffer
    fn client_pool_peer(&self) -> &Arc<ConnectedPeer<StoredMessage<PM::Message>>> {
        &self.client
    }

    /// Get the handle to the receiver for transmission
    fn to_send_handle(&self) -> &ChannelMixedRx<NetworkSerializedMessage> {
        &self.rx
    }
}

/// Stores all of the connections that this peer currently has established.
#[derive(Clone)]
pub struct PeerConnections<NI, RM, PM>
    where NI: NetworkInformationProvider + 'static,
          RM: Serializable + 'static,
          PM: Serializable + 'static {
    id: NodeId,
    first_cli: NodeId,
    concurrent_conn: ConnCounts,
    address_management: Arc<NI>,
    connection_map: Arc<DashMap<NodeId, Arc<PeerConnection<RM, PM>>>>,
    client_pooling: Arc<PeerIncomingRqHandling<StoredMessage<PM::Message>>>,
    reconf_handling: Arc<ReconfigurationMessageHandler<StoredMessage<RM::Message>>>,
    connection_establisher: Arc<ConnectionHandler>,
}

impl<NI, RM, PM> NodeConnections for PeerConnections<NI, RM, PM>
    where
        NI: NetworkInformationProvider + 'static,
        RM: Serializable + 'static,
        PM: Serializable + 'static {
    fn is_connected_to_node(&self, node: &NodeId) -> bool {
        self.connection_map.contains_key(node)
    }

    fn connected_nodes_count(&self) -> usize {
        self.connection_map.len()
    }

    fn connected_nodes(&self) -> Vec<NodeId> {
        let mut nodes = Vec::with_capacity(self.connection_map.len());

        self.connection_map.iter().for_each(|node| {
            nodes.push(node.key().clone());
        });

        nodes
    }

    /// Connect to a given node
    fn connect_to_node(self: &Arc<Self>, node: NodeId) -> Vec<OneShotRx<Result<()>>> {
        let option = self.get_addr_for_node(node);

        match option {
            None => {
                let (tx, rx) = new_oneshot_channel();

                tx.send(Err(Error::simple(ErrorKind::CommunicationPeerNotFound))).unwrap();

                vec![rx]
            }
            Some(addr) => {
                let conns_to_have = self.concurrent_conn.get_connections_to_node(self.id, node, self.first_cli);
                let connections = self.current_connection_count_of(&node).unwrap_or(0);

                let mut oneshots = Vec::with_capacity(conns_to_have);

                for _ in connections..conns_to_have {
                    oneshots.push(self.connection_establisher.connect_to_node(self, node, addr.clone()));
                }

                oneshots
            }
        }
    }

    /// Disconnected from a given node
    async fn disconnect_from_node(&self, node: &NodeId) -> Result<()> {
        if let Some((id, conn)) = self.connection_map.remove(node) {
            todo!();

            Ok(())
        } else {
            Err(Error::simple(ErrorKind::CommunicationPeerNotFound))
        }
    }
}

impl<NI, RM, PM> PeerConnections<NI, RM, PM>
    where NI: NetworkInformationProvider + 'static,
          RM: Serializable + 'static,
          PM: Serializable + 'static {
    pub fn new(peer_id: NodeId, first_cli: NodeId,
               conn_counts: ConnCounts,
               node_lookup: Arc<NI>,
               node_connector: TlsNodeConnector,
               node_acceptor: TlsNodeAcceptor,
               client_pooling: Arc<PeerIncomingRqHandling<StoredMessage<PM::Message>>>,
               reconf_handling: Arc<ReconfigurationMessageHandler<StoredMessage<RM::Message>>>)
               -> Arc<Self> {
        let connection_establish = ConnectionHandler::new(peer_id, first_cli, conn_counts.clone(),
                                                          node_connector, node_acceptor);

        Arc::new(Self {
            id: peer_id,
            first_cli,
            concurrent_conn: conn_counts,
            address_management: node_lookup,
            connection_map: Arc::new(DashMap::new()),
            client_pooling,
            reconf_handling,
            connection_establisher: connection_establish,
        })
    }

    pub(crate) fn id(&self) -> NodeId {
        self.id.clone()
    }

    fn get_addr_for_node(&self, node: NodeId) -> Option<PeerAddr> {
        self.address_management.get_addr_for_node(&node)
    }

    /// Setup a tcp listener inside this peer connections object.
    pub(super) fn setup_tcp_listener(self: Arc<Self>, node_acceptor: NodeConnectionAcceptor) {
        self.connection_establisher.clone().setup_conn_worker(node_acceptor, self)
    }

    /// Get the current amount of concurrent TCP connections between nodes
    pub fn current_connection_count_of(&self, node: &NodeId) -> Option<usize> {
        self.connection_map.get(node).map(|connection| {
            connection.value().connection_count()
        })
    }

    /// Get the connection to a given node
    pub fn get_connection(&self, node: &NodeId) -> Option<Arc<PeerConnection<RM, PM>>> {
        let option = self.connection_map.get(node);

        option.map(|conn| conn.value().clone())
    }

    /// Handle a new connection being established (either through a "client" or a "server" connection)
    /// This will either create the corresponding peer connection or add the connection to the already existing
    /// connection
    pub(crate) fn handle_connection_established(self: &Arc<Self>,
                                                node: NodeId, socket: (SecureWriteHalf, SecureReadHalf)) {
        debug!("{:?} // Handling established connection to {:?}", self.id, node);

        let option = self.connection_map.entry(node);

        let peer_conn = option.or_insert_with(||
            {
                let con = PeerConnection::new_peer(
                    self.id,
                    self.client_pooling.init_peer_conn(node),
                    self.reconf_handling.clone());

                debug!("{:?} // Creating new peer connection to {:?}. {:?}", self.id, node,
                    con.client_pool_peer().client_id());

                con
            });

        let concurrency_level = self.concurrent_conn.get_connections_to_node(self.id, node, self.first_cli);

        peer_conn.insert_new_connection(self.clone(), socket, concurrency_level);
    }

    /// Handle us losing a TCP connection to a given node.
    /// Also accepts the current amount of connections available in that node
    fn handle_conn_lost(self: &Arc<Self>, node: &NodeId, remaining_conns: usize) {
        let concurrency_level = self.concurrent_conn.get_connections_to_node(self.id, node.clone(), self.first_cli);

        if remaining_conns <= 0 {
            //The node is no longer accessible. We will remove it until a new TCP connection
            // Has been established
            let _ = self.connection_map.remove(node);
        }

        // Attempt to re-establish all of the missing connections
        if remaining_conns < concurrency_level {
            let addr = self.get_addr_for_node(node.clone()).unwrap();

            for _ in 0..concurrency_level - remaining_conns {
                self.connection_establisher.connect_to_node(self, node.clone(), addr.clone());
            }
        }
    }
}
