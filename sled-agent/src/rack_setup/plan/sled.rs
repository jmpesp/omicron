// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Plan generation for "how should sleds be initialized".

use crate::bootstrap::config::BOOTSTRAP_AGENT_RACK_INIT_PORT;
use omicron_uuid_kinds::SledUuid;
use sled_agent_types::rack_init::RackInitializeRequest as Config;
use sled_agent_types::sled::StartSledAgentRequest;
use sled_agent_types::sled::StartSledAgentRequestBody;
use slog::Logger;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv6Addr, SocketAddrV6};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Plan {
    pub rack_id: Uuid,
    pub sleds: BTreeMap<SocketAddrV6, StartSledAgentRequest>,

    // Store the provided RSS configuration as part of the sled plan.
    pub config: Config,
}

impl Plan {
    pub fn create(
        log: &Logger,
        config: &Config,
        bootstrap_addrs: BTreeSet<Ipv6Addr>,
        use_trust_quorum: bool,
    ) -> Self {
        let rack_id = Uuid::new_v4();

        let bootstrap_addrs = bootstrap_addrs.into_iter().enumerate();
        let allocations = bootstrap_addrs.map(|(_, bootstrap_addr)| {
            info!(log, "Creating plan for the sled at {:?}", bootstrap_addr);
            let bootstrap_addr = SocketAddrV6::new(
                bootstrap_addr,
                BOOTSTRAP_AGENT_RACK_INIT_PORT,
                0,
                0,
            );

            // XXX normally, each sled gets whatever index. problem is that
            // we're not automatically discovering zpools for non-gimlets, so
            // dataset requests will need to go to the right machine. use `dladm
            // show-phys -m` to get MAC addresses and match here.
            use sled_hardware_types::underlay::mac_to_bootstrap_ip;
            let bootstrap_ipv6: Ipv6Addr = *bootstrap_addr.ip();
            let interface_id = 1;

            let idx =
                // dinnerbone ixgbe0
                if mac_to_bootstrap_ip("00:1b:21:c1:ff:e0".parse().unwrap(), interface_id) == bootstrap_ipv6 {
                    0

                // kibblesnbits ixgbe0
                } else if mac_to_bootstrap_ip("00:1b:21:c1:fc:da".parse().unwrap(), interface_id) == bootstrap_ipv6 {
                    1

                // gravytrain ixgbe0
                } else if mac_to_bootstrap_ip("00:1b:21:c1:fd:24".parse().unwrap(), interface_id) == bootstrap_ipv6 {
                    2

                // frostypaws ixgbe0 - ixgbe5
                } else if mac_to_bootstrap_ip("80:61:5f:07:41:d8".parse().unwrap(), interface_id) == bootstrap_ipv6 ||
                    mac_to_bootstrap_ip("80:61:5f:07:41:d9".parse().unwrap(), interface_id) == bootstrap_ipv6 ||
                    mac_to_bootstrap_ip("80:61:5f:11:ab:30".parse().unwrap(), interface_id) == bootstrap_ipv6 ||
                    mac_to_bootstrap_ip("80:61:5f:11:ab:31".parse().unwrap(), interface_id) == bootstrap_ipv6 ||
                    mac_to_bootstrap_ip("8c:dc:d4:af:d2:a8".parse().unwrap(), interface_id) == bootstrap_ipv6 ||
                    mac_to_bootstrap_ip("8c:dc:d4:af:d2:a9".parse().unwrap(), interface_id) == bootstrap_ipv6 {

                    3

                } else {
                    panic!("unrecognized bootstrap addr {:?}", bootstrap_addr);
                };

            let sled_subnet_index =
                u8::try_from(idx + 1).expect("Too many peers!");
            let subnet = config.sled_subnet(sled_subnet_index);

            (
                bootstrap_addr,
                StartSledAgentRequest {
                    generation: 0,
                    schema_version: 1,
                    body: StartSledAgentRequestBody {
                        id: SledUuid::new_v4(),
                        subnet,
                        use_trust_quorum,
                        is_lrtq_learner: false,
                        rack_id,
                    },
                },
            )
        });

        let mut sleds = BTreeMap::new();
        for (addr, allocation) in allocations {
            sleds.insert(addr, allocation);
        }

        Self { rack_id, sleds, config: config.clone() }
    }
}
