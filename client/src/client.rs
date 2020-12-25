//! Core nakamoto client functionality. Wraps all the other modules under a unified
//! interface.
use std::env;
use std::fs;
use std::io;
use std::net;
use std::ops::Range;
use std::path::PathBuf;
use std::time::{self, SystemTime};

use crossbeam_channel as chan;

use nakamoto_chain::block::cache::BlockCache;
use nakamoto_chain::block::store;
use nakamoto_chain::filter;
use nakamoto_chain::filter::cache::FilterCache;

use nakamoto_common::block::filter::Filters;
use nakamoto_common::block::store::Store;
use nakamoto_common::block::time::AdjustedTime;
use nakamoto_common::block::tree::{self, BlockTree, ImportResult};
use nakamoto_common::block::{Block, BlockHash, BlockHeader, Height, Transaction};

pub use nakamoto_common::network::Network;

use nakamoto_p2p as p2p;
use nakamoto_p2p::bitcoin::network::message::NetworkMessage;
use nakamoto_p2p::protocol::Command;
use nakamoto_p2p::protocol::Link;
use nakamoto_p2p::protocol::{connmgr, syncmgr};

pub use nakamoto_p2p::address_book::AddressBook;
pub use nakamoto_p2p::event::Event;
pub use nakamoto_p2p::reactor::Reactor;

use crate::error::Error;
use crate::handle::{self, Handle};

/// Client configuration.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Client listen addresses.
    pub listen: Vec<net::SocketAddr>,
    /// Bitcoin network.
    pub network: Network,
    /// Initial address book.
    pub address_book: AddressBook,
    /// Target number of outbound peers to connect to.
    pub target_outbound_peers: usize,
    /// Maximum number of inbound peers supported.
    pub max_inbound_peers: usize,
    /// Timeout duration for client commands.
    pub timeout: time::Duration,
    /// Client home path, where runtime data is stored, eg. block headers and filters.
    pub home: PathBuf,
    /// Client name. Used for logging only.
    pub name: &'static str,
}

#[cfg(test)]
impl ClientConfig {
    /// Create a default client configuration with a name.
    pub(crate) fn named(name: &'static str) -> Self {
        Self {
            name,
            ..Self::default()
        }
    }
}

impl From<ClientConfig> for p2p::protocol::Config {
    fn from(cfg: ClientConfig) -> Self {
        Self {
            network: cfg.network,
            target: cfg.name,
            address_book: cfg.address_book,
            target_outbound_peers: cfg.target_outbound_peers,
            max_inbound_peers: cfg.max_inbound_peers,
            ..Self::default()
        }
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            listen: vec![([0, 0, 0, 0], 0).into()],
            network: Network::default(),
            address_book: AddressBook::default(),
            timeout: time::Duration::from_secs(60),
            home: PathBuf::from(env::var("HOME").unwrap_or_default()),
            target_outbound_peers: p2p::protocol::connmgr::TARGET_OUTBOUND_PEERS,
            max_inbound_peers: p2p::protocol::connmgr::MAX_INBOUND_PEERS,
            name: "self",
        }
    }
}

/// A light-client process.
pub struct Client<R> {
    /// Client configuration.
    pub config: ClientConfig,

    handle: chan::Sender<Command>,
    events: chan::Receiver<Event>,
    reactor: R,
}

impl<R: Reactor> Client<R> {
    /// Create a new client.
    pub fn new(config: ClientConfig) -> Result<Self, Error> {
        let (handle, commands) = chan::unbounded::<Command>();
        let (subscriber, events) = chan::unbounded::<Event>();
        let reactor = R::new(subscriber, commands)?;

        Ok(Self {
            events,
            handle,
            reactor,
            config,
        })
    }

    /// Seed the client's address book with peer addresses.
    pub fn seed<S: net::ToSocketAddrs>(&mut self, seeds: Vec<S>) -> Result<(), Error> {
        self.config.address_book.seed(seeds).map_err(Error::from)
    }

    /// Start the client process. This function is meant to be run in its own thread.
    pub fn run(mut self) -> Result<(), Error> {
        let home = self.config.home.join(".nakamoto");
        let dir = home.join(self.config.network.as_str());
        let listen = self.config.listen.clone();

        fs::create_dir_all(&dir)?;

        let genesis = self.config.network.genesis();
        let params = self.config.network.params();

        log::info!("Initializing client ({:?})..", self.config.network);
        log::info!(
            "Genesis block hash is {}",
            self.config.network.genesis_hash()
        );

        let path = dir.join("headers.db");
        let mut store = match store::File::create(&path, genesis) {
            Err(store::Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                log::info!("Found existing store {:?}", path);
                store::File::open(path, genesis)?
            }
            Err(err) => panic!(err.to_string()),
            Ok(store) => {
                log::info!("Initializing new block store {:?}", path);
                store
            }
        };
        if store.check().is_err() {
            log::warn!("Corruption detected in store, healing..");
            store.heal()?; // Rollback store to the last valid header.
        }
        log::info!("Store height = {}", store.height()?);
        log::info!("Loading blocks from store..");

        let local_time = SystemTime::now().into();
        let checkpoints = self.config.network.checkpoints().collect::<Vec<_>>();
        let clock = AdjustedTime::<net::SocketAddr>::new(local_time);
        let cache = BlockCache::from(store, params, &checkpoints)?;
        let rng = fastrand::Rng::new();

        log::info!("Initializing block filters..");

        let cfheaders_genesis = filter::cache::StoredHeader::genesis(self.config.network);
        let cfheaders_path = dir.join("filters.db");
        let cfheaders_store = match store::File::create(&cfheaders_path, cfheaders_genesis) {
            Err(store::Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                log::info!("Found existing store {:?}", cfheaders_path);
                store::File::open(cfheaders_path, cfheaders_genesis)?
            }
            Err(err) => panic!(err.to_string()),
            Ok(store) => {
                log::info!("Initializing new filter header store {:?}", cfheaders_path);
                store
            }
        };
        let filters = FilterCache::new(cfheaders_store);

        filters.verify()?; // Verify store integrity.

        log::info!("{} peer(s) found..", self.config.address_book.len());
        log::debug!("{:?}", self.config.address_book);

        let cfg = p2p::protocol::Config {
            network: self.config.network,
            params: self.config.network.params(),
            target: self.config.name,
            address_book: self.config.address_book,
            target_outbound_peers: self.config.target_outbound_peers,
            max_inbound_peers: self.config.max_inbound_peers,
            ..p2p::protocol::Config::default()
        };
        let builder = p2p::protocol::Builder {
            cache,
            clock,
            filters,
            rng,
            cfg,
        };

        self.reactor.run(builder, &listen)?;

        Ok(())
    }

    /// Start the client process, supplying the block cache. This function is meant to be run in
    /// its own thread.
    pub fn run_with<T: BlockTree, F: Filters>(mut self, cache: T, filters: F) -> Result<(), Error> {
        let cfg = p2p::protocol::Config::from(
            self.config.name,
            self.config.network,
            self.config.address_book,
        );

        log::info!("Initializing client ({:?})..", cfg.network);
        log::info!("Genesis block hash is {}", cfg.network.genesis_hash());
        log::info!("Chain height is {}", cache.height());

        let local_time = SystemTime::now().into();
        let clock = AdjustedTime::<net::SocketAddr>::new(local_time);
        let rng = fastrand::Rng::new();

        log::info!("{} peer(s) found..", cfg.address_book.len());
        log::debug!("{:?}", cfg.address_book);

        let builder = p2p::protocol::Builder {
            cache,
            clock,
            filters,
            rng,
            cfg,
        };

        self.reactor.run(builder, &self.config.listen)?;

        Ok(())
    }

    /// Create a new handle to communicate with the client.
    pub fn handle(&self) -> ClientHandle<R> {
        ClientHandle {
            waker: self.reactor.waker(),
            commands: self.handle.clone(),
            events: self.events.clone(),
            timeout: self.config.timeout,
        }
    }
}

/// An instance of [`Handle`] for [`Client`].
pub struct ClientHandle<R: Reactor> {
    commands: chan::Sender<Command>,
    events: chan::Receiver<Event>,
    waker: R::Waker,
    timeout: time::Duration,
}

impl<R: Reactor> ClientHandle<R> {
    /// Set the timeout for operations that wait on the network.
    pub fn set_timeout(&mut self, timeout: time::Duration) {
        self.timeout = timeout;
    }

    /// Send a command to the command channel, and wake up the event loop.
    pub fn command(&self, cmd: Command) -> Result<(), handle::Error> {
        self.commands.send(cmd)?;
        R::wake(&self.waker)?;

        Ok(())
    }
}

impl<R: Reactor> Handle for ClientHandle<R> {
    fn get_tip(&self) -> Result<BlockHeader, handle::Error> {
        let (transmit, receive) = chan::bounded::<BlockHeader>(1);
        self.command(Command::GetTip(transmit))?;

        Ok(receive.recv()?)
    }

    fn get_block(&self, hash: &BlockHash) -> Result<Block, handle::Error> {
        self.command(Command::GetBlock(*hash))?;
        self.wait(|e| match e {
            Event::Received(_, NetworkMessage::Block(blk)) if &blk.block_hash() == hash => {
                Some(blk)
            }
            _ => None,
        })
    }

    fn get_filters(&self, range: Range<Height>) -> Result<(), handle::Error> {
        assert!(
            !range.is_empty(),
            "ClientHandle::get_filters: range cannot be empty"
        );
        self.command(Command::GetFilters(range))
    }

    fn broadcast(&self, msg: NetworkMessage) -> Result<(), handle::Error> {
        self.command(Command::Broadcast(msg))
    }

    fn query(&self, msg: NetworkMessage) -> Result<Option<net::SocketAddr>, handle::Error> {
        let (transmit, receive) = chan::bounded::<Option<net::SocketAddr>>(1);
        self.command(Command::Query(msg, transmit))?;

        Ok(receive.recv()?)
    }

    fn connect(&self, addr: net::SocketAddr) -> Result<Link, handle::Error> {
        self.command(Command::Connect(addr))?;
        self.wait(|e| match e {
            Event::ConnManager(connmgr::Event::Connected(a, link))
                if a == addr || (addr.ip().is_unspecified() && a.port() == addr.port()) =>
            {
                Some(link)
            }
            _ => None,
        })
    }

    fn disconnect(&self, addr: net::SocketAddr) -> Result<(), handle::Error> {
        self.command(Command::Disconnect(addr))?;
        self.wait(|e| match e {
            Event::ConnManager(connmgr::Event::Disconnected(a))
                if a == addr || (addr.ip().is_unspecified() && a.port() == addr.port()) =>
            {
                Some(())
            }
            _ => None,
        })
    }

    fn import_headers(
        &self,
        headers: Vec<BlockHeader>,
    ) -> Result<Result<ImportResult, tree::Error>, handle::Error> {
        let (transmit, receive) = chan::bounded::<Result<ImportResult, tree::Error>>(1);
        self.command(Command::ImportHeaders(headers, transmit))?;

        Ok(receive.recv()?)
    }

    fn submit_transaction(&self, tx: Transaction) -> Result<(), handle::Error> {
        self.command(Command::SubmitTransaction(tx))?;

        Ok(())
    }

    /// Subscribe to the event feed, and wait for the given function to return something,
    /// or timeout if the specified amount of time has elapsed.
    fn wait<F, T>(&self, f: F) -> Result<T, handle::Error>
    where
        F: Fn(Event) -> Option<T>,
    {
        let start = time::Instant::now();
        let events = self.events.clone();

        loop {
            if let Some(timeout) = self.timeout.checked_sub(start.elapsed()) {
                match events.recv_timeout(timeout) {
                    Ok(event) => {
                        if let Some(t) = f(event) {
                            return Ok(t);
                        }
                    }
                    Err(chan::RecvTimeoutError::Disconnected) => {
                        return Err(handle::Error::Disconnected);
                    }
                    Err(chan::RecvTimeoutError::Timeout) => {
                        // Keep trying until our timeout reaches zero.
                        continue;
                    }
                }
            } else {
                return Err(handle::Error::Timeout);
            }
        }
    }

    fn wait_for_peers(&self, count: usize) -> Result<(), handle::Error> {
        use std::collections::HashSet;

        self.wait(|e| {
            let mut connected = HashSet::new();

            match e {
                Event::ConnManager(connmgr::Event::Connected(addr, _)) => {
                    connected.insert(addr);

                    if connected.len() == count {
                        Some(())
                    } else {
                        None
                    }
                }
                _ => None,
            }
        })
    }

    fn wait_for_ready(&self) -> Result<(), handle::Error> {
        self.wait(|e| match e {
            Event::SyncManager(syncmgr::Event::Synced(_, _)) => Some(()),
            _ => None,
        })
    }

    fn wait_for_height(&self, h: Height) -> Result<BlockHash, handle::Error> {
        self.wait(|e| match e {
            Event::SyncManager(syncmgr::Event::HeadersImported(ImportResult::TipChanged(
                hash,
                height,
                _,
            ))) if height == h => Some(hash),
            _ => None,
        })
    }

    fn shutdown(self) -> Result<(), handle::Error> {
        self.command(Command::Shutdown)?;

        Ok(())
    }
}
