// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod command;
mod error;
mod event;
mod msg;

pub use self::{
    event::NetworkEvent,
    msg::{Request, Response},
};

use self::{
    command::SwarmCmd,
    error::Result,
    event::NodeBehaviour,
    msg::{MsgCodec, MsgProtocol},
};
use futures::{
    channel::{mpsc, oneshot},
    prelude::*,
};
use libp2p::{
    core::muxing::StreamMuxerBox,
    identity,
    kad::{record::store::MemoryStore, Kademlia, KademliaConfig, QueryId},
    mdns,
    request_response::{self, ProtocolSupport, RequestId, ResponseChannel},
    swarm::{Swarm, SwarmBuilder},
    Multiaddr, PeerId, Transport,
};
use std::{
    collections::{HashMap, HashSet},
    iter,
    time::Duration,
};
use tracing::warn;
use xor_name::XorName;

/// The main event loop recieves `SwarmEvents` from the network, `SwarmCmd` from the upper layers and
/// emmits back `NetworkEvent` to the upper layers.
/// Also keeps track of the pending queries/requests and their channels. Once we recieve an event
/// that is the outcome of a previously executed cmd, send a response to them via the stored channel.
pub struct NetworkSwarmLoop {
    swarm: Swarm<NodeBehaviour>,
    cmd_receiver: mpsc::Receiver<SwarmCmd>,
    event_sender: mpsc::Sender<NetworkEvent>,
    pending_dial: HashMap<PeerId, oneshot::Sender<Result<()>>>,
    pending_start_providing: HashMap<QueryId, oneshot::Sender<Result<()>>>,
    pending_get_providers: HashMap<QueryId, oneshot::Sender<HashSet<PeerId>>>,
    pending_requests: HashMap<RequestId, oneshot::Sender<Result<Response>>>,
}

impl NetworkSwarmLoop {
    /// Creates the network components
    /// - The `Network` to interact with the network layer from anywhere
    ///   within your application.
    ///
    /// - The `NetworkEvent` receiver to get the events from the network layer.
    ///
    /// - The `NetworkSwarmLoop` that drives the network.
    pub fn new() -> Result<(Network, impl Stream<Item = NetworkEvent>, NetworkSwarmLoop)> {
        // Create a random key for ourselves.
        let keypair = identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(keypair.public());

        // QUIC configuration
        let quic_config = libp2p_quic::Config::new(&keypair);
        let transport = libp2p_quic::async_std::Transport::new(quic_config);
        let transport = transport
            .map(|(peer_id, muxer), _| (peer_id, StreamMuxerBox::new(muxer)))
            .boxed();
        // Create a Kademlia instance and connect to the network address.
        // Create a swarm to manage peers and events.
        let swarm = {
            // Create a Kademlia behaviour.
            let mut cfg = KademliaConfig::default();
            let _ = cfg.set_query_timeout(Duration::from_secs(5 * 60));
            let kademlia =
                Kademlia::with_config(local_peer_id, MemoryStore::new(local_peer_id), cfg);
            let mdns = mdns::async_io::Behaviour::new(mdns::Config::default(), local_peer_id)?;
            let behaviour = NodeBehaviour {
                request_response: request_response::Behaviour::new(
                    MsgCodec(),
                    iter::once((MsgProtocol(), ProtocolSupport::Full)),
                    Default::default(),
                ),
                kademlia,
                mdns,
            };

            let mut swarm =
                SwarmBuilder::with_async_std_executor(transport, behaviour, local_peer_id).build();

            // Listen on all interfaces and whatever port the OS assigns.
            let addr = "/ip4/0.0.0.0/udp/0/quic-v1"
                .parse()
                .expect("Failed to parse the address");
            let _listener_id = swarm
                .listen_on(addr)
                .expect("Failed to listen on the provided address");

            swarm
        };

        let (swarm_cmd_sender, swarm_cmd_receiver) = mpsc::channel(0);
        let (event_sender, event_receiver) = mpsc::channel(0);
        let event_loop = Self {
            swarm,
            cmd_receiver: swarm_cmd_receiver,
            event_sender,
            pending_dial: Default::default(),
            pending_start_providing: Default::default(),
            pending_get_providers: Default::default(),
            pending_requests: Default::default(),
        };

        Ok((Network { swarm_cmd_sender }, event_receiver, event_loop))
    }

    /// Drive the network
    pub async fn run(mut self) {
        loop {
            futures::select! {
                event = self.swarm.next() => {
                    if let Err(err) = self.handle_event(event.expect("Swarm stream to be infinite!")).await {
                        warn!("Error while handling event: {err}");
                    }
                }  ,
                command = self.cmd_receiver.next() => match command {
                    Some(cmd) => {
                        if let Err(err) = self.handle_command(cmd) {
                            warn!("Error while handling cmd: {err}");
                        }
                    },
                    // Command channel closed, thus shutting down the network event loop.
                    None=>  return,
                },
            }
        }
    }
}

#[derive(Clone)]
/// API to interact with the underlying Swarm
pub struct Network {
    pub(super) swarm_cmd_sender: mpsc::Sender<SwarmCmd>,
}

impl Network {
    ///  Listen for incoming connections on the given address.
    pub async fn start_listening(&mut self, addr: Multiaddr) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.swarm_cmd_sender
            .send(SwarmCmd::StartListening { addr, sender })
            .await?;
        receiver.await?
    }

    /// Dial the given peer at the given address.
    pub async fn dial(&mut self, peer_id: PeerId, peer_addr: Multiaddr) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.swarm_cmd_sender
            .send(SwarmCmd::Dial {
                peer_id,
                peer_addr,
                sender,
            })
            .await?;
        receiver.await?
    }

    /// Advertise the local node as the provider of a given piece of data; The XorName of the data
    /// is advertised to the nodes on the DHT
    /// todo: do not use the provider api to store stuff
    pub async fn store_data(&mut self, xor_name: XorName) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.swarm_cmd_sender
            .send(SwarmCmd::StoreData { xor_name, sender })
            .await?;
        receiver.await?
    }

    /// Find the providers for the given piece of data; The XorName is used to locate the nodes
    /// that hold the data
    /// todo: do not use the provider api to store stuff
    pub async fn get_data_providers(&mut self, xor_name: XorName) -> Result<HashSet<PeerId>> {
        let (sender, receiver) = oneshot::channel();
        self.swarm_cmd_sender
            .send(SwarmCmd::GetDataProviders { xor_name, sender })
            .await?;
        Ok(receiver.await?)
    }

    /// Send `Request` to the the given `PeerId`
    pub async fn send_request(&mut self, req: Request, peer: PeerId) -> Result<Response> {
        let (sender, receiver) = oneshot::channel();
        self.swarm_cmd_sender
            .send(SwarmCmd::SendRequest { req, peer, sender })
            .await?;
        receiver.await?
    }

    /// Send a `Response` through the channel opened by the requester.
    pub async fn send_response(
        &mut self,
        resp: Response,
        channel: ResponseChannel<Response>,
    ) -> Result<()> {
        Ok(self
            .swarm_cmd_sender
            .send(SwarmCmd::SendResponse { resp, channel })
            .await?)
    }
}
