// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Manges deployment of Omicron physical disks to Sled Agents.

use crate::Sled;
use anyhow::Context;
use anyhow::anyhow;
use futures::StreamExt;
use futures::stream;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db::DataStore;
use nexus_types::deployment::Blueprint;
use nexus_types::deployment::BlueprintPhysicalDiskDisposition;
use nexus_types::deployment::BlueprintPhysicalDisksConfig;
use omicron_uuid_kinds::GenericUuid;
use omicron_uuid_kinds::PhysicalDiskUuid;
use omicron_uuid_kinds::SledUuid;
use slog::info;
use slog::o;
use slog::warn;
use std::collections::BTreeMap;

/// Idempotently ensure that the specified Omicron disks are deployed to the
/// corresponding sleds
pub(crate) async fn deploy_disks<'a, I>(
    opctx: &OpContext,
    sleds_by_id: &BTreeMap<SledUuid, Sled>,
    sled_configs: I,
) -> Result<(), Vec<anyhow::Error>>
where
    I: Iterator<Item = (SledUuid, &'a BlueprintPhysicalDisksConfig)>,
{
    let errors: Vec<_> = stream::iter(sled_configs)
        .filter_map(|(sled_id, config)| async move {
            let log = opctx.log.new(o!(
                "sled_id" => sled_id.to_string(),
                "generation" => config.generation.to_string(),
            ));

            let db_sled = match sleds_by_id.get(&sled_id) {
                Some(sled) => sled,
                None => {
                    if config.are_all_disks_expunged() {
                        info!(
                            log,
                            "Skipping disk deployment to expunged sled";
                            "sled_id" => %sled_id
                        );
                        return None;
                    }
                    let err = anyhow!("sled not found in db list: {}", sled_id);
                    warn!(log, "{err:#}");
                    return Some(err);
                }
            };

            let client = nexus_networking::sled_client_from_address(
                sled_id.into_untyped_uuid(),
                db_sled.sled_agent_address(),
                &log,
            );
            let result = client
                .omicron_physical_disks_put(
                    &config.clone().into_in_service_disks(),
                )
                .await
                .with_context(|| {
                    format!("Failed to put {config:#?} to sled {sled_id}")
                });
            match result {
                Err(error) => {
                    warn!(log, "{error:#}");
                    Some(error)
                }
                Ok(result) => {
                    let (errs, successes): (Vec<_>, Vec<_>) = result
                        .into_inner()
                        .status
                        .into_iter()
                        .partition(|status| status.err.is_some());

                    if !errs.is_empty() {
                        warn!(
                            log,
                            "Failed to deploy physical disk for sled agent";
                            "successfully configured disks" => successes.len(),
                            "failed disk configurations" => errs.len(),
                        );
                        for err in &errs {
                            warn!(log, "{err:?}");
                        }
                        return Some(anyhow!(
                            "failure deploying disks: {:?}",
                            errs
                        ));
                    }

                    info!(
                        log,
                        "Successfully deployed physical disks for sled agent";
                        "successfully configured disks" => successes.len(),
                    );
                    None
                }
            }
        })
        .collect()
        .await;

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

/// Decommissions all disks which are currently expunged.
pub(crate) async fn decommission_expunged_disks(
    opctx: &OpContext,
    datastore: &DataStore,
    blueprint: &Blueprint,
) -> Result<(), Vec<anyhow::Error>> {
    decommission_expunged_disks_impl(
        opctx,
        datastore,
        blueprint
            .all_omicron_disks(
                BlueprintPhysicalDiskDisposition::is_ready_for_cleanup,
            )
            .map(|(sled_id, config)| (sled_id, config.id)),
    )
    .await
}

async fn decommission_expunged_disks_impl(
    opctx: &OpContext,
    datastore: &DataStore,
    expunged_disks: impl Iterator<Item = (SledUuid, PhysicalDiskUuid)>,
) -> Result<(), Vec<anyhow::Error>> {
    let errors: Vec<anyhow::Error> = stream::iter(expunged_disks)
        .filter_map(|(sled_id, disk_id)| async move {
            let log = opctx.log.new(slog::o!(
                "sled_id" => sled_id.to_string(),
                "disk_id" => disk_id.to_string(),
            ));

            match datastore.physical_disk_decommission(&opctx, disk_id).await {
                Err(error) => {
                    warn!(
                        log,
                        "failed to decommission expunged disk";
                        "error" => #%error
                    );
                    Some(anyhow!(error).context(format!(
                        "failed to decommission: disk_id = {disk_id}",
                    )))
                }
                Ok(()) => {
                    info!(log, "successfully decommissioned expunged disk");
                    None
                }
            }
        })
        .collect()
        .await;

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

#[cfg(test)]
mod test {
    use super::deploy_disks;

    use crate::DataStore;
    use crate::Sled;
    use async_bb8_diesel::AsyncRunQueryDsl;
    use diesel::ExpressionMethods;
    use diesel::QueryDsl;
    use httptest::Expectation;
    use httptest::matchers::{all_of, json_decoded, request};
    use httptest::responders::json_encoded;
    use httptest::responders::status_code;
    use nexus_db_model::CrucibleDataset;
    use nexus_db_model::PhysicalDisk;
    use nexus_db_model::PhysicalDiskKind;
    use nexus_db_model::PhysicalDiskPolicy;
    use nexus_db_model::PhysicalDiskState;
    use nexus_db_model::Region;
    use nexus_db_model::Zpool;
    use nexus_db_queries::context::OpContext;
    use nexus_db_queries::db;
    use nexus_sled_agent_shared::inventory::SledRole;
    use nexus_test_utils::SLED_AGENT_UUID;
    use nexus_test_utils_macros::nexus_test;
    use nexus_types::deployment::BlueprintDatasetsConfig;
    use nexus_types::deployment::BlueprintPhysicalDiskDisposition;
    use nexus_types::deployment::BlueprintSledConfig;
    use nexus_types::deployment::BlueprintZonesConfig;
    use nexus_types::deployment::{
        Blueprint, BlueprintPhysicalDiskConfig, BlueprintPhysicalDisksConfig,
        BlueprintTarget, CockroachDbPreserveDowngrade, DiskFilter,
    };
    use nexus_types::external_api::views::SledState;
    use nexus_types::identity::Asset;
    use omicron_common::api::external::DataPageParams;
    use omicron_common::api::external::Generation;
    use omicron_common::disk::DiskIdentity;
    use omicron_common::disk::DiskManagementError;
    use omicron_common::disk::DiskManagementStatus;
    use omicron_common::disk::DisksManagementResult;
    use omicron_common::disk::OmicronPhysicalDisksConfig;
    use omicron_uuid_kinds::BlueprintUuid;
    use omicron_uuid_kinds::DatasetUuid;
    use omicron_uuid_kinds::GenericUuid;
    use omicron_uuid_kinds::PhysicalDiskUuid;
    use omicron_uuid_kinds::SledUuid;
    use omicron_uuid_kinds::VolumeUuid;
    use omicron_uuid_kinds::ZpoolUuid;
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use uuid::Uuid;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<omicron_nexus::Server>;

    fn create_blueprint(
        blueprint_disks: BTreeMap<SledUuid, BlueprintPhysicalDisksConfig>,
    ) -> (BlueprintTarget, Blueprint) {
        let id = BlueprintUuid::new_v4();
        (
            BlueprintTarget {
                target_id: id,
                enabled: true,
                time_made_target: chrono::Utc::now(),
            },
            Blueprint {
                id,
                sleds: blueprint_disks
                    .into_iter()
                    .map(|(sled_id, disks_config)| {
                        (
                            sled_id,
                            BlueprintSledConfig {
                                state: SledState::Active,
                                disks_config,
                                zones_config: BlueprintZonesConfig::default(),
                                datasets_config:
                                    BlueprintDatasetsConfig::default(),
                            },
                        )
                    })
                    .collect(),
                cockroachdb_setting_preserve_downgrade:
                    CockroachDbPreserveDowngrade::DoNotModify,
                parent_blueprint_id: None,
                internal_dns_version: Generation::new(),
                external_dns_version: Generation::new(),
                cockroachdb_fingerprint: String::new(),
                clickhouse_cluster_config: None,
                time_created: chrono::Utc::now(),
                creator: "test".to_string(),
                comment: "test blueprint".to_string(),
            },
        )
    }

    #[nexus_test]
    async fn test_deploy_omicron_disks(cptestctx: &ControlPlaneTestContext) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        // Create some fake sled-agent servers to respond to disk puts and add
        // sleds to CRDB.
        let mut s1 = httptest::Server::run();
        let mut s2 = httptest::Server::run();
        let sled_id1 = SledUuid::new_v4();
        let sled_id2 = SledUuid::new_v4();
        let sleds_by_id: BTreeMap<SledUuid, Sled> =
            [(sled_id1, &s1), (sled_id2, &s2)]
                .into_iter()
                .map(|(sled_id, server)| {
                    let SocketAddr::V6(addr) = server.addr() else {
                        panic!("Expected Ipv6 address. Got {}", server.addr());
                    };
                    let sled = Sled::new(sled_id, addr, SledRole::Gimlet);
                    (sled_id, sled)
                })
                .collect();

        // Get a success result back when the blueprint has an empty set of
        // disks.
        let (_, blueprint) = create_blueprint(BTreeMap::new());
        deploy_disks(
            &opctx,
            &sleds_by_id,
            blueprint
                .sleds
                .iter()
                .map(|(sled_id, config)| (*sled_id, &config.disks_config)),
        )
        .await
        .expect("failed to deploy no disks");

        // Disks are updated in a particular order, but each request contains
        // the full set of disks that must be running.
        // See `rack_setup::service::ServiceInner::run` for more details.
        fn make_disks() -> BlueprintPhysicalDisksConfig {
            BlueprintPhysicalDisksConfig {
                generation: Generation::new().next(),
                disks: [BlueprintPhysicalDiskConfig {
                    disposition: BlueprintPhysicalDiskDisposition::InService,
                    identity: DiskIdentity {
                        vendor: "test-vendor".to_string(),
                        serial: "test-serial".to_string(),
                        model: "test-model".to_string(),
                    },
                    id: PhysicalDiskUuid::new_v4(),
                    pool_id: ZpoolUuid::new_v4(),
                }]
                .into_iter()
                .collect(),
            }
        }

        // Create a blueprint with only one disk for both servers
        // We reuse the same `OmicronDisksConfig` because the details don't
        // matter for this test.
        let disks1 = make_disks();
        let disks2 = make_disks();
        let (_, blueprint) = create_blueprint(BTreeMap::from([
            (sled_id1, disks1.clone()),
            (sled_id2, disks2.clone()),
        ]));

        // Set expectations for the initial requests sent to the fake
        // sled-agents.
        for s in [&mut s1, &mut s2] {
            s.expect(
                Expectation::matching(all_of![
                    request::method_path("PUT", "/omicron-physical-disks",),
                    // Our generation number should be 2 and there should
                    // be only a single disk.
                    request::body(json_decoded(
                        |c: &OmicronPhysicalDisksConfig| {
                            c.generation == 2u32.into() && c.disks.len() == 1
                        }
                    ))
                ])
                .respond_with(json_encoded(
                    DisksManagementResult { status: vec![] },
                )),
            );
        }

        // Execute it.
        deploy_disks(
            &opctx,
            &sleds_by_id,
            blueprint
                .sleds
                .iter()
                .map(|(sled_id, config)| (*sled_id, &config.disks_config)),
        )
        .await
        .expect("failed to deploy initial disks");

        s1.verify_and_clear();
        s2.verify_and_clear();

        // Do it again. This should trigger the same request.
        for s in [&mut s1, &mut s2] {
            s.expect(
                Expectation::matching(request::method_path(
                    "PUT",
                    "/omicron-physical-disks",
                ))
                .respond_with(json_encoded(
                    DisksManagementResult { status: vec![] },
                )),
            );
        }
        deploy_disks(
            &opctx,
            &sleds_by_id,
            blueprint
                .sleds
                .iter()
                .map(|(sled_id, config)| (*sled_id, &config.disks_config)),
        )
        .await
        .expect("failed to deploy same disks");
        s1.verify_and_clear();
        s2.verify_and_clear();

        // Take another lap, but this time, have one server fail the request and
        // try again.
        s1.expect(
            Expectation::matching(request::method_path(
                "PUT",
                "/omicron-physical-disks",
            ))
            .respond_with(json_encoded(DisksManagementResult {
                status: vec![],
            })),
        );
        s2.expect(
            Expectation::matching(request::method_path(
                "PUT",
                "/omicron-physical-disks",
            ))
            .respond_with(status_code(500)),
        );

        let errors = deploy_disks(
            &opctx,
            &sleds_by_id,
            blueprint
                .sleds
                .iter()
                .map(|(sled_id, config)| (*sled_id, &config.disks_config)),
        )
        .await
        .expect_err("unexpectedly succeeded in deploying disks");

        println!("{:?}", errors);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .starts_with("Failed to put BlueprintPhysicalDisksConfig")
        );
        s1.verify_and_clear();
        s2.verify_and_clear();

        // We can also observe "partial failures", where the HTTP-evel response
        // is successful, but it indicates that the disk provisioning ran into
        // problems.
        s1.expect(
            Expectation::matching(request::method_path(
                "PUT",
                "/omicron-physical-disks",
            ))
            .respond_with(json_encoded(DisksManagementResult {
                status: vec![],
            })),
        );
        s2.expect(
            Expectation::matching(request::method_path(
                "PUT",
                "/omicron-physical-disks",
            ))
            .respond_with(json_encoded(DisksManagementResult {
                status: vec![DiskManagementStatus {
                    identity: omicron_common::disk::DiskIdentity {
                        vendor: "v".to_string(),
                        serial: "s".to_string(),
                        model: "m".to_string(),
                    },

                    // This error could occur if a disk is removed
                    err: Some(DiskManagementError::NotFound),
                }],
            })),
        );

        let errors = deploy_disks(
            &opctx,
            &sleds_by_id,
            blueprint
                .sleds
                .iter()
                .map(|(sled_id, config)| (*sled_id, &config.disks_config)),
        )
        .await
        .expect_err("unexpectedly succeeded in deploying disks");

        println!("{:?}", errors);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].to_string().starts_with("failure deploying disks"),
            "{}",
            errors[0].to_string()
        );
        s1.verify_and_clear();
        s2.verify_and_clear();
    }

    async fn make_disk_in_db(
        datastore: &DataStore,
        opctx: &OpContext,
        i: usize,
        sled_id: SledUuid,
    ) -> PhysicalDiskUuid {
        let id = PhysicalDiskUuid::new_v4();
        let physical_disk = PhysicalDisk::new(
            id,
            "v".into(),
            format!("s-{i})"),
            "m".into(),
            PhysicalDiskKind::U2,
            sled_id.into_untyped_uuid(),
        );
        datastore
            .physical_disk_insert(&opctx, physical_disk.clone())
            .await
            .unwrap();
        id
    }

    async fn add_zpool_dataset_and_region(
        datastore: &DataStore,
        opctx: &OpContext,
        id: PhysicalDiskUuid,
        sled_id: SledUuid,
    ) {
        let zpool = datastore
            .zpool_insert(
                opctx,
                Zpool::new(Uuid::new_v4(), sled_id.into_untyped_uuid(), id),
            )
            .await
            .unwrap();

        let dataset = datastore
            .crucible_dataset_upsert(CrucibleDataset::new(
                DatasetUuid::new_v4(),
                zpool.id(),
                std::net::SocketAddrV6::new(
                    std::net::Ipv6Addr::LOCALHOST,
                    0,
                    0,
                    0,
                ),
            ))
            .await
            .unwrap();

        // There isn't a great API to insert regions (we normally allocate!)
        // so insert the record manually here.
        let region = {
            let volume_id = VolumeUuid::new_v4();
            Region::new(
                dataset.id(),
                volume_id,
                512_i64.try_into().unwrap(),
                10,
                10,
                1,
                false,
            )
        };
        let conn = datastore.pool_connection_for_tests().await.unwrap();
        use nexus_db_model::schema::region::dsl;
        diesel::insert_into(dsl::region)
            .values(region)
            .execute_async(&*conn)
            .await
            .unwrap();
    }

    async fn get_pools(
        datastore: &DataStore,
        id: PhysicalDiskUuid,
    ) -> Vec<ZpoolUuid> {
        let conn = datastore.pool_connection_for_tests().await.unwrap();

        use db::schema::zpool::dsl;
        dsl::zpool
            .filter(dsl::time_deleted.is_null())
            .filter(dsl::physical_disk_id.eq(id.into_untyped_uuid()))
            .select(dsl::id)
            .load_async::<Uuid>(&*conn)
            .await
            .map(|ids| {
                ids.into_iter()
                    .map(|id| ZpoolUuid::from_untyped_uuid(id))
                    .collect()
            })
            .unwrap()
    }

    async fn get_crucible_datasets(
        datastore: &DataStore,
        id: ZpoolUuid,
    ) -> Vec<Uuid> {
        let conn = datastore.pool_connection_for_tests().await.unwrap();

        use db::schema::crucible_dataset::dsl;
        dsl::crucible_dataset
            .filter(dsl::time_deleted.is_null())
            .filter(dsl::pool_id.eq(id.into_untyped_uuid()))
            .select(dsl::id)
            .load_async(&*conn)
            .await
            .unwrap()
    }

    async fn get_regions(datastore: &DataStore, id: Uuid) -> Vec<Uuid> {
        let conn = datastore.pool_connection_for_tests().await.unwrap();

        use db::schema::region::dsl;
        dsl::region
            .filter(dsl::dataset_id.eq(id.into_untyped_uuid()))
            .select(dsl::id)
            .load_async(&*conn)
            .await
            .unwrap()
    }

    #[nexus_test]
    async fn test_decommission_expunged_disks(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        let sled_id = SledUuid::from_untyped_uuid(
            Uuid::from_str(&SLED_AGENT_UUID).unwrap(),
        );

        // Create a couple disks in the database
        let disks = [
            make_disk_in_db(&datastore, &opctx, 0, sled_id).await,
            make_disk_in_db(&datastore, &opctx, 1, sled_id).await,
        ];

        // Add a zpool, dataset, and region to each disk.
        for disk_id in disks {
            add_zpool_dataset_and_region(&datastore, &opctx, disk_id, sled_id)
                .await;
        }

        let disk_to_decommission = disks[0];
        let other_disk = disks[1];

        // Expunge one of the disks
        datastore
            .physical_disk_update_policy(
                &opctx,
                disk_to_decommission,
                PhysicalDiskPolicy::Expunged,
            )
            .await
            .unwrap();

        // Verify that the state of both disks is "active"
        let all_disks = datastore
            .physical_disk_list(
                &opctx,
                &DataPageParams::max_page(),
                DiskFilter::All,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|disk| (disk.id(), disk))
            .collect::<BTreeMap<_, _>>();
        let d = &all_disks[&disk_to_decommission];
        assert_eq!(d.disk_state, PhysicalDiskState::Active);
        assert_eq!(d.disk_policy, PhysicalDiskPolicy::Expunged);
        let d = &all_disks[&other_disk];
        assert_eq!(d.disk_state, PhysicalDiskState::Active);
        assert_eq!(d.disk_policy, PhysicalDiskPolicy::InService);

        super::decommission_expunged_disks_impl(
            &opctx,
            &datastore,
            [(sled_id, disk_to_decommission)].into_iter(),
        )
        .await
        .unwrap();

        // After decommissioning, we see the expunged disk become
        // decommissioned. The other disk remains in-service.
        let all_disks = datastore
            .physical_disk_list(
                &opctx,
                &DataPageParams::max_page(),
                DiskFilter::All,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|disk| (disk.id(), disk))
            .collect::<BTreeMap<_, _>>();
        let d = &all_disks[&disk_to_decommission];
        assert_eq!(d.disk_state, PhysicalDiskState::Decommissioned);
        assert_eq!(d.disk_policy, PhysicalDiskPolicy::Expunged);
        let d = &all_disks[&other_disk];
        assert_eq!(d.disk_state, PhysicalDiskState::Active);
        assert_eq!(d.disk_policy, PhysicalDiskPolicy::InService);

        // Even though we've decommissioned this disk, the pools, datasets, and
        // regions should remain. Refer to the "decommissioned_disk_cleaner"
        // for how these get eventually cleared up.
        let pools = get_pools(&datastore, disk_to_decommission).await;
        assert_eq!(pools.len(), 1);
        let datasets = get_crucible_datasets(&datastore, pools[0]).await;
        assert_eq!(datasets.len(), 1);
        let regions = get_regions(&datastore, datasets[0]).await;
        assert_eq!(regions.len(), 1);

        // Similarly, the "other disk" should still exist.
        let pools = get_pools(&datastore, other_disk).await;
        assert_eq!(pools.len(), 1);
        let datasets = get_crucible_datasets(&datastore, pools[0]).await;
        assert_eq!(datasets.len(), 1);
        let regions = get_regions(&datastore, datasets[0]).await;
        assert_eq!(regions.len(), 1);
    }
}
