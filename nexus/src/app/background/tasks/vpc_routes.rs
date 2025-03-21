// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background task for propagating VPC routes (system and custom) to sleds.

use crate::app::background::BackgroundTask;
use futures::FutureExt;
use futures::future::BoxFuture;
use nexus_db_model::{Sled, SledState, Vni};
use nexus_db_queries::{context::OpContext, db::DataStore};
use nexus_networking::sled_client_from_address;
use nexus_types::{
    deployment::SledFilter, external_api::views::SledPolicy, identity::Asset,
    identity::Resource,
};
use omicron_common::api::internal::shared::{
    ExternalIpGatewayMap, ResolvedVpcRoute, ResolvedVpcRouteSet, RouterId,
    RouterKind, RouterVersion,
};
use serde_json::json;
use std::collections::hash_map::Entry;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use uuid::Uuid;

pub struct VpcRouteManager {
    datastore: Arc<DataStore>,
}

impl VpcRouteManager {
    pub fn new(datastore: Arc<DataStore>) -> Self {
        Self { datastore }
    }
}

// This RPW doesn't concern itself overly much with resolved router targets
// and destinations being partial wrt. the current generation, in the same
// vein as how firewall rules are handled. Gating *pushing* this update on a
// generation number can be a bit more risky, but there's a sort of eventual
// consistency happening here that keeps this safe.
//
// Any location which updates name-resolvable state follows the pattern:
//  * Update state.
//  * Update (VPC-wide) router generation numbers.
//  * Awaken this task. This might happen indirectly via e.g. instance start.
//
// As a result, any update which accidentally sees partial state will be followed
// by re-triggering this RPW with a higher generation number, giving us a re-resolved
// route set and pushing to any relevant sleds.
impl BackgroundTask for VpcRouteManager {
    fn activate<'a>(
        &'a mut self,
        opctx: &'a OpContext,
    ) -> BoxFuture<'a, serde_json::Value> {
        async {
            let log = &opctx.log;
            info!(log, "VPC route manager running");

            let sleds = match self
                .datastore
                .sled_list_all_batched(opctx, SledFilter::VpcRouting)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("failed to enumerate sleds: {:#}", e);
                    error!(&log, "{msg}");
                    return json!({"error": msg});
                }
            }
            .into_iter()
            .filter(|sled| {
                matches!(sled.state(), SledState::Active)
                    && matches!(sled.policy(), SledPolicy::InService { .. })
            });

            // Map sled db records to sled-agent clients
            let sled_clients: Vec<(Sled, sled_agent_client::Client)> = sleds
                .map(|sled| {
                    let client = sled_client_from_address(
                        sled.id(),
                        sled.address(),
                        &log,
                    );
                    (sled, client)
                })
                .collect();

            let mut known_rules: HashMap<Uuid, HashSet<ResolvedVpcRoute>> =
                HashMap::new();
            let mut db_routers = HashMap::new();
            let mut vni_to_vpc = HashMap::new();

            for (sled, client) in sled_clients {
                info!(log, "VPC route manager sled {}", sled.id());
                let Ok(route_sets) = client.list_vpc_routes().await else {
                    warn!(
                        log,
                        "failed to fetch current VPC route state from sled";
                        "sled" => sled.serial_number(),
                    );
                    continue;
                };

                // Map each external IP in use by the sled to the Internet Gateway(s)
                // which are allowed to make use of it.
                // TODO: this should really not be the responsibility of this RPW.
                // I would expect this belongs in a future external IPs RPW, but until
                // then it lives here since it's a core part of the Internet Gateways
                // system.
                match self.datastore.vpc_resolve_sled_external_ips_to_gateways(opctx, sled.id()).await {
                    Ok(mappings) => {
                        info!(
                            log,
                            "computed internet gateway mappings for sled";
                            "sled" => sled.serial_number(),
                            "assocs" => ?mappings
                        );
                        let param = ExternalIpGatewayMap {mappings};
                        if let Err(e) = client.set_eip_gateways(&param).await {
                            error!(
                                log,
                                "failed to push internet gateway assignments for sled";
                                "sled" => sled.serial_number(),
                                "err" => ?e
                            );
                            continue;
                        };
                    }
                    Err(e) => {
                        error!(
                            log,
                            "failed to produce EIP Internet Gateway mappings for sled";
                            "sled" => sled.serial_number(),
                            "err" => ?e
                        );
                    }
                }

                let route_sets = route_sets.into_inner();

                // Lookup all VPC<->Subnet<->Router associations we might need,
                // based on the set of VNIs reported by this sled.
                // These provide the versions we'll stick with -- in the worst
                // case we push newer state to a sled with an older generation
                // number, which will be fixed up on the next activation.
                for set in &route_sets {
                    let db_vni = Vni(set.id.vni);
                    let maybe_vpc = vni_to_vpc.entry(set.id.vni);
                    let vpc = match maybe_vpc {
                        Entry::Occupied(_) => {
                            continue;
                        }
                        Entry::Vacant(v) => {
                            let Ok(vpc) = self
                                .datastore
                                .resolve_vni_to_vpc(opctx, db_vni)
                                .await
                            else {
                                error!(
                                    log,
                                    "failed to fetch VPC from VNI";
                                    "sled" => sled.serial_number(),
                                    "vni" => ?db_vni
                                );
                                continue;
                            };

                            v.insert(vpc)
                        }
                    };

                    let vpc_id = vpc.identity().id;

                    let Ok(system_router) = self
                        .datastore
                        .vpc_get_system_router(opctx, vpc_id)
                        .await
                    else {
                        error!(
                            log,
                            "failed to fetch system router for VPC";
                            "vpc" => vpc_id.to_string()
                        );
                        continue;
                    };

                    let Ok(custom_routers) = self
                        .datastore
                        .vpc_get_active_custom_routers_with_associated_subnets(opctx, vpc_id)
                        .await
                    else {
                        error!(
                            log,
                            "failed to fetch custom routers for VPC";
                            "vpc" => vpc_id.to_string()
                        );
                        continue;
                    };

                    db_routers.insert(
                        RouterId { vni: set.id.vni, kind: RouterKind::System },
                        system_router,
                    );
                    db_routers.extend(custom_routers.iter().map(
                        |(subnet, router)| {
                            (
                                RouterId {
                                    vni: set.id.vni,
                                    kind: RouterKind::Custom(
                                        subnet.ipv4_block.0.into(),
                                    ),
                                },
                                router.clone(),
                            )
                        },
                    ));
                    db_routers.extend(custom_routers.into_iter().map(
                        |(subnet, router)| {
                            (
                                RouterId {
                                    vni: set.id.vni,
                                    kind: RouterKind::Custom(
                                        subnet.ipv6_block.0.into(),
                                    ),
                                },
                                router,
                            )
                        },
                    ));
                }

                let mut to_push = Vec::new();
                let mut set_rules = |id, version, routes| {
                    to_push.push(ResolvedVpcRouteSet { id, routes, version });
                };

                // resolve into known_rules on an as-needed basis.
                for set in &route_sets {
                   let Some(db_router) = db_routers.get(&set.id) else {
                        // The sled wants to know about rules for a VPC
                        // subnet with no custom router set. Send them
                        // the empty list, and unset its table version.
                        set_rules(set.id, None, HashSet::new());
                        continue;
                    };

                    let router_id = db_router.id();
                    let version = RouterVersion {
                        version: db_router.resolved_version as u64,
                        router_id,
                    };

                    // Only attempt to resolve/push a ruleset if we have a
                    // different router ID than the sled, or a higher version
                    // number.
                    match &set.version {
                        Some(v) if !v.is_replaced_by(&version) => {
                            info!(
                                log,
                                "VPC route manager sled {} push not needed",
                                sled.id()
                            );
                            continue;
                            // Currently, this version is bumped in response to
                            // events that change the routes. This has to be
                            // done explicitly by the programmer in response to
                            // any event that may result in a difference in
                            // route resolution calculation. With the
                            // introduction of internet gateway targets that
                            // are parameterized on source IP, route resolution
                            // computations can change based on events that are
                            // not directly modifying VPC routes - like the
                            // linkiage of an IP pool to a silo, or the
                            // allocation of an external IP address and
                            // attachment of that IP address to a service or
                            // instance. This broadened context for changes
                            // influencing route resolution makes manual
                            // tracking of a router version easy to get wrong
                            // and I feel like it will be a bug magnet.
                            //
                            // I think we should move decisions around change
                            // propagation to be based on actual delta
                            // calculation, rather than trying to manually
                            // maintain a signal.
                        }
                        _ => {}
                    }

                    // We may have already resolved the rules for this
                    // router in a previous iteration.
                    if let Some(rules) = known_rules.get(&router_id) {
                        info!(
                            log,
                            "VPC route manager sled {} rules already resolved",
                            sled.id()
                        );
                        set_rules(set.id, Some(version), rules.clone());
                    }

                    match self
                        .datastore
                        .vpc_resolve_router_rules(
                            opctx,
                            db_router.identity().id,
                        )
                        .await
                    {
                        Ok(rules) => {
                            set_rules(set.id, Some(version), rules.clone());
                            known_rules.insert(router_id, rules);
                        }
                        Err(e) => {
                            error!(
                                &log,
                                "failed to compute subnet routes";
                                "router" => router_id.to_string(),
                                "err" => e.to_string()
                            );
                        }
                    }
                }

                if !to_push.is_empty() {
                    if let Err(e) = client.set_vpc_routes(&to_push).await {
                        error!(
                            log,
                            "failed to push new VPC route state from sled";
                            "sled" => sled.serial_number(),
                            "err" => ?e
                        );
                        continue;
                    };
                }
            }

            json!({})
        }
        .boxed()
    }
}
