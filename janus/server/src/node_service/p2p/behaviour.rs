/*
 * Copyright 2020 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::node_service::function::FunctionRouter;
use crate::node_service::Bootstrapper;

use faas_api::FunctionCall;
use janus_libp2p::{event_polling, generate_swarm_event_type};
use libp2p::{
    identify::Identify,
    identity::{ed25519, PublicKey},
    ping::{Ping, PingConfig, PingEvent},
    PeerId,
};
use parity_multiaddr::Multiaddr;
use std::collections::VecDeque;

mod bootstrapper;
mod identify;

pub type SwarmEventType = generate_swarm_event_type!(P2PBehaviour);

/// Coordinates protocols, so they can cooperate
#[derive(::libp2p::NetworkBehaviour)]
#[behaviour(poll_method = "custom_poll")]
pub struct P2PBehaviour {
    bootstrapper: Bootstrapper,
    router: FunctionRouter,
    identity: Identify,
    ping: Ping,
    #[behaviour(ignore)]
    events: VecDeque<SwarmEventType>,
}

impl P2PBehaviour {
    pub fn new(
        key_pair: ed25519::Keypair,
        local_peer_id: PeerId,
        root_weights: Vec<(ed25519::PublicKey, u32)>,
        bootstrap_nodes: Vec<Multiaddr>,
    ) -> Self {
        let router = FunctionRouter::new(key_pair.clone(), local_peer_id, root_weights);
        let local_public_key = PublicKey::Ed25519(key_pair.public());
        let identity = Identify::new("/janus/faas/1.0.0".into(), "0.1.0".into(), local_public_key);
        let ping = Ping::new(PingConfig::new().with_keep_alive(false));
        let bootstrapper = Bootstrapper::new(bootstrap_nodes);

        Self {
            router,
            identity,
            ping,
            bootstrapper,
            events: Default::default(),
        }
    }

    /// Bootstraps the node. Currently, does nothing.
    pub fn bootstrap(&mut self) {
        // TODO: how to avoid collect?
        let bootstraps: Vec<_> = self.bootstrapper.bootstrap_nodes.iter().cloned().collect();
        for maddr in bootstraps {
            self.dial(maddr)
        }
    }

    pub fn call(&mut self, call: FunctionCall) {
        self.router.call(call)
    }

    fn dial(&mut self, maddr: Multiaddr) {
        self.events
            .push_back(libp2p::swarm::NetworkBehaviourAction::DialAddress {
                address: maddr.clone(),
            })
    }

    fn dial_peer(&mut self, peer_id: PeerId) {
        self.events
            .push_back(libp2p::swarm::NetworkBehaviourAction::DialPeer {
                peer_id,
                condition: libp2p::swarm::DialPeerCondition::Disconnected,
            });
    }

    event_polling!(custom_poll, events, SwarmEventType);
}

impl libp2p::swarm::NetworkBehaviourEventProcess<()> for P2PBehaviour {
    fn inject_event(&mut self, _: ()) {}
}

impl libp2p::swarm::NetworkBehaviourEventProcess<PingEvent> for P2PBehaviour {
    fn inject_event(&mut self, _: PingEvent) {}
}
