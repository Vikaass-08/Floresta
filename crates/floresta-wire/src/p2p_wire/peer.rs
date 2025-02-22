use self::peer_utils::make_pong;
use super::{
    mempool::Mempool,
    node::{NodeNotification, NodeRequest},
    stream_reader::StreamReader,
};
use thiserror::Error;

use async_std::{
    channel::{unbounded, Receiver, Sender},
    io::BufReader,
    net::{TcpStream, ToSocketAddrs},
    sync::RwLock,
    task::spawn,
};
use bitcoin::{
    consensus::serialize,
    hashes::Hash,
    network::{
        address::AddrV2Message,
        constants::ServiceFlags,
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::Inventory,
        message_network::VersionMessage,
        utreexo::UtreexoBlock,
    },
    BlockHash, BlockHeader, Network, Transaction,
};
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt, FutureExt};
use log::{debug, error, warn};
use std::{
    fmt::Debug,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq)]
enum State {
    None,
    SentVersion(Instant),
    SentVerack,
    Connected,
}
/// A trait defining how the transport we use should behave. Transport is anything
/// that allows to read/write from/into. Like a TcpStream or a Socks5 proxy
pub trait Transport:
    AsyncRead + AsyncWrite + Unpin + Clone + Sync + Send + AsyncWriteExt + 'static
{
    /// Asks the stream to shutdown, the final part of the disconnection process
    fn shutdown(&mut self) -> Result<()>;
}

impl Transport for TcpStream {
    fn shutdown(&mut self) -> Result<()> {
        Ok(TcpStream::shutdown(self, std::net::Shutdown::Both)?)
    }
}

pub struct Peer<T: Transport> {
    stream: T,
    mempool: Arc<RwLock<Mempool>>,
    network: Network,
    blocks_only: bool,
    services: ServiceFlags,
    user_agent: String,
    messages: u64,
    start_time: Instant,
    current_best_block: i32,
    last_ping: Instant,
    id: u32,
    node_tx: Sender<NodeNotification>,
    state: State,
    send_headers: bool,
    node_requests: Receiver<NodeRequest>,
    address_id: usize,
    feeler: bool,
    wants_addrv2: bool,
}
#[derive(Debug, Error)]
pub enum PeerError {
    #[error("Error while sending to peer")]
    Send,
    #[error("Error while reading from peer")]
    Read(#[from] std::io::Error),
    #[error("Error while parsing message")]
    Parse(#[from] bitcoin::consensus::encode::Error),
    #[error("Peer sent us a message that we aren't expecting")]
    UnexpectedMessage,
    #[error("Peer sent us a message that is too big")]
    MessageTooBig,
    #[error("Peer sent us a message with the wrong magic bits")]
    MagicBitsMismatch,
    #[error("Peer sent us too many message in a short period of time")]
    TooManyMessages,
}
impl Debug for Peer<TcpStream> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.id)?;
        write!(f, "{:?}", self.stream.peer_addr())?;
        Ok(())
    }
}
type Result<T> = std::result::Result<T, PeerError>;

impl<T: Transport> Peer<T> {
    pub async fn read_loop(mut self) -> Result<()> {
        let err = self.peer_loop_inner().await;
        debug!("Peer connection loop closed: {err:?}");
        self.send_to_node(PeerMessages::Disconnected(self.address_id))
            .await;
        Ok(())
    }
    async fn peer_loop_inner(&mut self) -> Result<()> {
        // send a version
        let version = peer_utils::build_version_message();
        self.write(version).await?;
        self.state = State::SentVersion(Instant::now());
        let read_stream = BufReader::new(self.stream.clone());
        let (tx, rx) = unbounded();
        let stream: StreamReader<_, RawNetworkMessage> =
            StreamReader::new(read_stream, self.network.magic(), tx);
        spawn(stream.read_loop());
        loop {
            futures::select! {
                request = self.node_requests.recv().fuse() => {
                    if let Ok(request) = request {
                        self.handle_node_request(request).await?;
                    }
                }
                peer_request = async_std::future::timeout(Duration::from_secs(10), rx.recv()).fuse() => {
                    if let Ok(Ok(peer_request)) = peer_request {
                        self.handle_peer_message(peer_request?).await?;
                    }
                }
            };
            // divide the number of messages by the number of seconds we've been connected,
            // if it's more than 100 msg/sec, this peer is sending us too many messages, and we should
            // disconnect.
            if self.messages > 0
                && self.messages / Instant::now().duration_since(self.start_time).as_secs() > 10
            {
                error!(
                    "Peer {} is sending us too many messages, disconnecting",
                    self.id
                );
                return Err(PeerError::TooManyMessages);
            }
            if let State::SentVersion(when) = self.state {
                if Instant::now().duration_since(when) > Duration::from_secs(10) {
                    return Err(PeerError::UnexpectedMessage);
                }
            }
        }
    }
    pub async fn handle_node_request(&mut self, request: NodeRequest) -> Result<()> {
        match request {
            NodeRequest::GetBlock((block_hashes, proof)) => {
                let inv = if proof {
                    block_hashes
                        .iter()
                        .map(|block| Inventory::UtreexoWitnessBlock(*block))
                        .collect()
                } else {
                    block_hashes
                        .iter()
                        .map(|block| Inventory::Block(*block))
                        .collect()
                };

                let _ = self.write(NetworkMessage::GetData(inv)).await;
            }
            NodeRequest::GetHeaders(locator) => {
                let _ = self
                    .write(NetworkMessage::GetHeaders(
                        bitcoin::network::message_blockdata::GetHeadersMessage {
                            version: 0,
                            locator_hashes: locator,
                            stop_hash: BlockHash::all_zeros(),
                        },
                    ))
                    .await;
            }
            NodeRequest::Shutdown => {
                let _ = self.stream.shutdown();
            }
            NodeRequest::GetAddresses => {
                self.write(NetworkMessage::GetAddr).await?;
            }
            NodeRequest::BroadcastTransaction(tx) => {
                self.write(NetworkMessage::Inv(vec![Inventory::Transaction(tx)]))
                    .await?;
            }
            NodeRequest::MempoolTransaction(txid) => {
                self.write(NetworkMessage::GetData(vec![Inventory::Transaction(txid)]))
                    .await?;
            }
            NodeRequest::SendAddresses(addresses) => {
                self.write(NetworkMessage::AddrV2(addresses)).await?;
            }
        }
        Ok(())
    }
    pub async fn handle_peer_message(&mut self, message: RawNetworkMessage) -> Result<()> {
        match self.state {
            State::Connected => match message.payload {
                NetworkMessage::Inv(inv) => {
                    for inv_entry in inv {
                        match inv_entry {
                            Inventory::Error => {}
                            Inventory::Transaction(_) => {}
                            Inventory::Block(block_hash)
                            | Inventory::WitnessBlock(block_hash)
                            | Inventory::CompactBlock(block_hash) => {
                                self.send_to_node(PeerMessages::NewBlock(block_hash)).await;
                            }
                            _ => {}
                        }
                    }
                }
                NetworkMessage::GetHeaders(_) => {
                    self.write(NetworkMessage::Headers(Vec::new())).await?;
                }
                NetworkMessage::Block(block) => {
                    self.send_to_node(PeerMessages::Block(block)).await;
                }
                NetworkMessage::Headers(headers) => {
                    self.send_to_node(PeerMessages::Headers(headers)).await;
                }
                NetworkMessage::SendHeaders => {
                    self.send_headers = true;
                    self.write(NetworkMessage::SendHeaders).await?;
                }
                NetworkMessage::Ping(nonce) => {
                    self.handle_ping(nonce).await?;
                }
                NetworkMessage::FeeFilter(_) => {
                    self.write(NetworkMessage::FeeFilter(1000)).await?;
                }
                NetworkMessage::AddrV2(addresses) => {
                    self.send_to_node(PeerMessages::Addr(addresses)).await;
                }
                NetworkMessage::GetBlocks(_) => {
                    self.write(NetworkMessage::Inv(Vec::new())).await?;
                }
                NetworkMessage::GetAddr => {
                    self.write(NetworkMessage::AddrV2(Vec::new())).await?;
                }
                NetworkMessage::GetData(inv) => {
                    for inv_el in inv {
                        self.handle_get_data(inv_el).await?;
                    }
                }
                NetworkMessage::Tx(tx) => {
                    self.send_to_node(PeerMessages::Transaction(tx)).await;
                }
                NetworkMessage::NotFound(inv) => {
                    for inv_el in inv {
                        self.send_to_node(PeerMessages::NotFound(inv_el)).await;
                    }
                }
                NetworkMessage::SendAddrV2 => {
                    self.wants_addrv2 = true;
                    self.write(NetworkMessage::SendAddrV2).await?;
                }
                NetworkMessage::Unknown { command, payload } => {
                    warn!("Unknown message: {} {:?}", command, payload);
                }

                // Explicitly ignore these messages, if something changes in the future
                // this would cause a compile error.
                NetworkMessage::Verack
                | NetworkMessage::Version(_)
                | NetworkMessage::WtxidRelay
                | NetworkMessage::Reject(_)
                | NetworkMessage::Alert(_)
                | NetworkMessage::BlockTxn(_)
                | NetworkMessage::CFCheckpt(_)
                | NetworkMessage::CFHeaders(_)
                | NetworkMessage::CFilter(_)
                | NetworkMessage::CmpctBlock(_)
                | NetworkMessage::FilterAdd(_)
                | NetworkMessage::FilterClear
                | NetworkMessage::FilterLoad(_)
                | NetworkMessage::GetBlockTxn(_)
                | NetworkMessage::GetCFCheckpt(_)
                | NetworkMessage::GetCFHeaders(_)
                | NetworkMessage::Addr(_)
                | NetworkMessage::GetCFilters(_)
                | NetworkMessage::MemPool
                | NetworkMessage::MerkleBlock(_)
                | NetworkMessage::Pong(_)
                | NetworkMessage::SendCmpct(_) => {}
            },
            State::None | State::SentVersion(_) => match message.payload {
                bitcoin::network::message::NetworkMessage::Version(version) => {
                    self.handle_version(version).await?;
                    self.send_to_node(PeerMessages::Ready(Version {
                        user_agent: self.user_agent.clone(),
                        protocol_version: 0,
                        id: self.id,
                        blocks: self.current_best_block.unsigned_abs(),
                        address_id: self.address_id,
                        services: self.services,
                        feeler: self.feeler,
                    }))
                    .await;
                }
                _ => {
                    warn!(
                        "unexpected message: {:?} from peer {}",
                        message.payload, self.id
                    );
                    return Err(PeerError::UnexpectedMessage);
                }
            },
            State::SentVerack => match message.payload {
                bitcoin::network::message::NetworkMessage::Verack => {
                    self.state = State::Connected;
                }
                bitcoin::network::message::NetworkMessage::SendAddrV2 => {
                    self.wants_addrv2 = true;
                }
                bitcoin::network::message::NetworkMessage::SendHeaders => {
                    self.send_headers = true;
                }
                bitcoin::network::message::NetworkMessage::WtxidRelay => {}
                _ => {
                    warn!(
                        "unexpected message: {:?} from peer {}",
                        message.payload, self.id
                    );
                    return Err(PeerError::UnexpectedMessage);
                }
            },
        }
        Ok(())
    }
}
impl<T: Transport> Peer<T> {
    pub async fn write(&mut self, msg: NetworkMessage) -> Result<()> {
        let data = &mut RawNetworkMessage {
            magic: self.network.magic(),
            payload: msg,
        };
        let data = serialize(&data);
        self.stream.write_all(data.as_slice()).await?;
        Ok(())
    }
    pub async fn handle_get_data(&mut self, inv: Inventory) -> Result<()> {
        match inv {
            Inventory::WitnessTransaction(txid) => {
                let tx = self.mempool.read().await.get_from_mempool(&txid).cloned();
                if let Some(tx) = tx {
                    self.write(NetworkMessage::Tx(tx)).await?;
                }
            }
            Inventory::Transaction(txid) => {
                let tx = self.mempool.read().await.get_from_mempool(&txid).cloned();
                if let Some(tx) = tx {
                    self.write(NetworkMessage::Tx(tx)).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn create_peer_from_transport(
        stream: T,
        id: u32,
        mempool: Arc<RwLock<Mempool>>,
        network: Network,
        node_tx: Sender<NodeNotification>,
        node_requests: Receiver<NodeRequest>,
        address_id: usize,
        feeler: bool,
    ) {
        let peer = Peer {
            address_id,
            blocks_only: false,
            current_best_block: -1,
            id,
            mempool,
            last_ping: Instant::now(),
            network,
            node_tx,
            services: ServiceFlags::NONE,
            stream,
            messages: 0,
            start_time: Instant::now(),
            user_agent: "".into(),
            state: State::None,
            send_headers: false,
            node_requests,
            feeler,
            wants_addrv2: false,
        };
        spawn(peer.read_loop());
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_outbound_connection<A: ToSocketAddrs + Debug>(
        id: u32,
        address: A,
        mempool: Arc<RwLock<Mempool>>,
        network: Network,
        node_tx: Sender<NodeNotification>,
        node_requests: Receiver<NodeRequest>,
        address_id: usize,
        feeler: bool,
    ) {
        let stream =
            async_std::future::timeout(Duration::from_secs(10), TcpStream::connect(address)).await;
        let Ok(Ok(stream)) = stream else {
            let _ = node_tx
                .send(NodeNotification::FromPeer(
                    id,
                    PeerMessages::Disconnected(id as usize),
                ))
                .await;
            return;
        };
        let peer = Peer {
            address_id,
            blocks_only: false,
            current_best_block: -1,
            id,
            mempool,
            last_ping: Instant::now(),
            network,
            node_tx,
            services: ServiceFlags::NONE,
            stream,
            messages: 0,
            start_time: Instant::now(),
            user_agent: "".into(),
            state: State::None,
            send_headers: false,
            node_requests,
            feeler,
            wants_addrv2: false,
        };
        spawn(peer.read_loop());
    }
    async fn handle_ping(&mut self, nonce: u64) -> Result<()> {
        self.last_ping = Instant::now();
        let pong = make_pong(nonce);
        self.write(pong).await
    }
    async fn handle_version(&mut self, version: VersionMessage) -> Result<()> {
        self.user_agent = version.user_agent;
        self.blocks_only = !version.relay;
        self.current_best_block = version.start_height;
        self.services = version.services;
        if version.version >= 70016 {
            self.write(NetworkMessage::SendAddrV2).await?;
        }
        self.state = State::SentVerack;
        let verack = NetworkMessage::Verack;
        self.state = State::SentVerack;
        self.write(verack).await
    }
    async fn send_to_node(&self, message: PeerMessages) {
        let message = NodeNotification::FromPeer(self.id, message);
        let _ = self.node_tx.send(message).await;
    }
}
pub(super) mod peer_utils {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        time::{SystemTime, UNIX_EPOCH},
    };

    use bitcoin::network::{
        address, constants,
        message::{self, NetworkMessage},
        message_network,
    };
    use floresta_common::constants::{FLORESTA_VERSION, RUSTREEXO_VERSION, RUST_BITCOIN_VERSION};

    /// Protocol version we speak
    pub const PROTOCOL_VERSION: u32 = 70016;

    pub(super) fn make_pong(nonce: u64) -> NetworkMessage {
        NetworkMessage::Pong(nonce)
    }
    pub(super) fn build_version_message() -> message::NetworkMessage {
        use bitcoin::network::constants::ServiceFlags;
        // Building version message, see https://en.bitcoin.it/wiki/Protocol_documentation#version
        let my_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 38332);

        // "bitfield of features to be enabled for this connection"
        let services = ServiceFlags::NETWORK | ServiceFlags::NODE_UTREEXO | ServiceFlags::WITNESS;

        // "standard UNIX timestamp in seconds"
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time error")
            .as_secs();

        // "The network address of the node receiving this message"
        let addr_recv = address::Address::new(&my_address, constants::ServiceFlags::NONE);

        // "The network address of the node emitting this message"
        let addr_from = address::Address::new(&my_address, constants::ServiceFlags::NONE);

        // "Node random nonce, randomly generated every time a version packet is sent. This nonce is used to detect connections to self."
        let nonce: u64 = 1;

        // "User Agent (0x00 if string is 0 bytes long)"
        let user_agent = format!(
            "/rust-bitcoin:{}/rustreexo:{}/Floresta:{}/",
            RUST_BITCOIN_VERSION, RUSTREEXO_VERSION, FLORESTA_VERSION
        );

        // "The last block received by the emitting node"
        let start_height: i32 = 0;

        // Construct the message
        message::NetworkMessage::Version(message_network::VersionMessage {
            services,
            timestamp: timestamp as i64,
            receiver: addr_recv,
            sender: addr_from,
            nonce,
            user_agent,
            start_height,
            relay: false,
            version: PROTOCOL_VERSION,
        })
    }
}
#[derive(Debug)]
pub struct Version {
    pub user_agent: String,
    pub protocol_version: u32,
    pub blocks: u32,
    pub id: u32,
    pub address_id: usize,
    pub services: ServiceFlags,
    pub feeler: bool,
}
/// Messages passed from different modules to the main node to process. They should minimal
/// and only if it requires global states, everything else should be handled by the module
/// itself.
#[derive(Debug)]
pub enum PeerMessages {
    /// A new block just arrived, we should ask for it and update our chain
    NewBlock(BlockHash),
    /// We got a full block from our peer, presumptively we asked for it
    Block(UtreexoBlock),
    /// A response to a `getheaders` request
    Headers(Vec<BlockHeader>),
    /// We got some p2p addresses, add this to our local database
    Addr(Vec<AddrV2Message>),
    /// Peer notify its readiness
    Ready(Version),
    /// Remote peer disconnected
    Disconnected(usize),
    /// Remote peer doesn't known the data we asked for
    NotFound(Inventory),
    /// Remote peer sent us a transaction
    Transaction(Transaction),
}
