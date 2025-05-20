// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XXX TODO

use super::{
    common_storage::{
        call_pantry_attach_for_volume, call_pantry_detach,
        get_pantry_address, is_pantry_gone,
    },
    ActionRegistry, NexusActionContext, NexusSaga, SagaInitError,
    ACTION_GENERATE_ID,
};
use crate::app::sagas::declare_saga_actions;
use crate::app::{authn, authz, db};
use crate::external_api::params;
use anyhow::anyhow;
use nexus_db_model::Generation;
use nexus_db_queries::db::identity::{Asset, Resource};
use nexus_db_lookup::LookupPath;
use omicron_common::api::external::Error;
use omicron_common::progenitor_operation_retry::ProgenitorOperationRetryError;
use omicron_common::{
    api::external, progenitor_operation_retry::ProgenitorOperationRetry,
};
use omicron_uuid_kinds::{GenericUuid, PropolisUuid, SledUuid, VolumeUuid};
use omicron_uuid_kinds::UserDataExportUuid;
use rand::{rngs::StdRng, RngCore, SeedableRng};
use serde::Deserialize;
use serde::Serialize;
use sled_agent_client::types::VmmIssueDiskSnapshotRequestBody;
use sled_agent_client::CrucibleOpts;
use sled_agent_client::VolumeConstructionRequest;
use slog::info;
use slog_error_chain::InlineErrorChain;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::net::SocketAddrV6;
use steno::ActionError;
use steno::Node;
use uuid::Uuid;

// snapshot export create saga: input parameters

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Params {
    pub serialized_authn: authn::saga::Serialized,
    //pub project_id: Uuid,
    pub snapshot_id: Uuid,
}

// snapshot export create saga: actions
declare_saga_actions! {
    user_data_export_create;
    CREATE_EXPORT_VOLUME -> "export_volume" {
        + ssec_create_export_volume
        - ssec_create_export_volume_undo
    }
    GET_PANTRY_ADDRESS -> "pantry_address" {
        + ssec_get_pantry_address
    }
    CALL_PANTRY_ATTACH_FOR_EXPORT -> "call_pantry_attach_for_export" {
        + ssec_call_pantry_attach_for_export
        - ssec_call_pantry_attach_for_export_undo
    }
    CREATE_EXPORT_RECORD -> "created_record" {
        + ssec_create_export_record
        - ssec_create_export_record_undo
    }
}

// snapshot export create saga: definition

#[derive(Debug)]
pub(crate) struct SagaUserDataExportCreate;
impl NexusSaga for SagaUserDataExportCreate {
    const NAME: &'static str = "snapshot-export-create";
    type Params = Params;

    fn register_actions(registry: &mut ActionRegistry) {
        user_data_export_create_register_actions(registry);
    }

    fn make_saga_dag(
        params: &Self::Params,
        mut builder: steno::DagBuilder,
    ) -> Result<steno::Dag, SagaInitError> {
        // Generate IDs
        builder.append(Node::action(
            "volume_id",
            "GenerateVolumeId",
            ACTION_GENERATE_ID.as_ref(),
        ));

        builder.append(Node::action(
            "user_data_export_id",
            "GenerateUserDataExportId",
            ACTION_GENERATE_ID.as_ref(),
        ));

        builder.append(create_export_volume_action());
        builder.append(get_pantry_address_action());
        builder.append(call_pantry_attach_for_export_action());
        builder.append(create_export_record_action());

        Ok(builder.build()?)
    }
}

// snapshot export saga: action implementations

async fn ssec_create_export_volume(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    debug!(log, "grabbing snapshot {}", params.snapshot_id);

    let (.., db_snapshot) =
        LookupPath::new(&opctx, osagactx.datastore())
            .snapshot_id(params.snapshot_id)
            .fetch()
            .await
            .map_err(ActionError::action_failed)?;

    let volume_id = sagactx.lookup::<VolumeUuid>("volume_id")?;

    debug!(
        log,
        "copying snapshot {} volume {} to {volume_id}",
        db_snapshot.id(),
        db_snapshot.volume_id(),
    );

    osagactx
        .datastore()
        .volume_checkout_randomize_ids(
            db::datastore::SourceVolume(db_snapshot.volume_id()),
            db::datastore::DestVolume(volume_id),
            db::datastore::VolumeCheckoutReason::ReadOnlyCopy,
        )
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

async fn ssec_create_export_volume_undo(
    sagactx: NexusActionContext,
) -> Result<(), anyhow::Error> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let volume_id = sagactx.lookup::<VolumeUuid>("volume_id")?;

    // `volume_create` will increase the resource count for read only resources
    // in a volume, which there are guaranteed to be for snapshot volumes.
    // decreasing crucible resources is necessary as an undo step. Do not call
    // `volume_hard_delete` here: soft deleting volumes is necessary for
    // `find_deleted_volume_regions` to work.

    info!(log, "calling soft delete for volume {}", volume_id);

    osagactx.datastore().soft_delete_volume(volume_id).await?;

    // XXX what about hard delete? bg task?

    Ok(())
}

async fn ssec_get_pantry_address(
    sagactx: NexusActionContext,
) -> Result<SocketAddrV6, ActionError> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;

    let pantry_address = get_pantry_address(osagactx.nexus()).await?;

    info!(log, "using pantry at {}", pantry_address);

    Ok(pantry_address)
}

async fn ssec_call_pantry_attach_for_export(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let volume_id = sagactx.lookup::<VolumeUuid>("volume_id")?;
    let pantry_address = sagactx.lookup::<SocketAddrV6>("pantry_address")?;

    let volume = match osagactx.datastore().volume_get(volume_id).await {
        Ok(Some(volume)) => volume,

        Ok(None) => {
            return Err(ActionError::action_failed(Error::internal_error(
                &format!("volume {volume_id} gone!")
            )));
        }

        Err(e) => {
            return Err(ActionError::action_failed(Error::internal_error(
                &format!("failed to get volume {volume_id}: {e}")
            )));
        }
    };

    let volume_construction_request =
        serde_json::from_str(&volume.data()).map_err(|e| {
            ActionError::action_failed(Error::internal_error(&format!(
                "failed to deserialize volume {volume_id} data: {e}"
            )))
        })?;

    info!(log, "sending attach for volume {volume_id} to {pantry_address}");

    call_pantry_attach_for_volume(
        &log,
        &osagactx.nexus(),
        volume_id.into_untyped_uuid(),
        volume_construction_request,
        pantry_address,
    )
    .await
}

async fn ssec_call_pantry_attach_for_export_undo(
    sagactx: NexusActionContext,
) -> Result<(), anyhow::Error> {
    let log = sagactx.user_data().log();
    let params = sagactx.saga_params::<Params>()?;

    let volume_id = sagactx.lookup::<VolumeUuid>("volume_id")?;
    let pantry_address = sagactx.lookup::<SocketAddrV6>("pantry_address")?;

    info!(log, "undo: detaching {volume_id} from pantry at {pantry_address}");

    match call_pantry_detach(
        sagactx.user_data().nexus(),
        &log,
        volume_id.into_untyped_uuid(),
        pantry_address,
    )
    .await
    {
        // We can treat the pantry being permanently gone as success.
        Ok(()) | Err(ProgenitorOperationRetryError::Gone) => Ok(()),

        Err(err) => Err(anyhow!(
            "failed to detach {volume_id} from pantry at {pantry_address}: {}",
            InlineErrorChain::new(&err)
        ))
    }
}

async fn ssec_create_export_record(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;

    let volume_id = sagactx.lookup::<VolumeUuid>("volume_id")?;
    let pantry_address = sagactx.lookup::<SocketAddrV6>("pantry_address")?;
    let user_data_export_id = sagactx.lookup::<UserDataExportUuid>("user_data_export_id")?;

    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let (.., authz_snapshot) =
        LookupPath::new(&opctx, osagactx.datastore())
            .snapshot_id(params.snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .expect("Failed to look up snapshot");

    let user_data_export = osagactx
        .datastore()
        .user_data_export_create(
            &opctx,
            &authz_snapshot,
            user_data_export_id,
            pantry_address,
            volume_id,
        )
        .await
        .map_err(ActionError::action_failed)?;

    info!(log, "snapshot export {} created ok", user_data_export.id());

    Ok(())
}

async fn ssec_create_export_record_undo(
    sagactx: NexusActionContext,
) -> Result<(), anyhow::Error> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;

    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let user_data_export_id = sagactx.lookup::<UserDataExportUuid>("user_data_export_id")?;

    let (.., authz_snapshot) =
        LookupPath::new(&opctx, osagactx.datastore())
            .snapshot_id(params.snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .expect("Failed to look up snapshot");

    info!(log, "calling delete export {user_data_export_id}");

    osagactx.datastore().user_data_export_delete(&opctx, &authz_snapshot, user_data_export_id).await?;

    Ok(())
}

/*
// XXX
#[cfg(test)]
mod test {
    use super::*;

    use crate::app::saga::create_saga_dag;
    use crate::app::sagas::test_helpers;
    use async_bb8_diesel::AsyncRunQueryDsl;
    use diesel::{
        ExpressionMethods, OptionalExtension, QueryDsl, SelectableHelper,
    };
    use dropshot::test_util::ClientTestContext;
    use nexus_db_queries::context::OpContext;
    use nexus_db_queries::db::datastore::InstanceAndActiveVmm;
    use nexus_db_queries::db::DataStore;
    use nexus_test_utils::resource_helpers::create_default_ip_pool;
    use nexus_test_utils::resource_helpers::create_disk;
    use nexus_test_utils::resource_helpers::create_project;
    use nexus_test_utils::resource_helpers::delete_disk;
    use nexus_test_utils::resource_helpers::object_create;
    use nexus_test_utils_macros::nexus_test;
    use nexus_types::external_api::params::InstanceDiskAttachment;
    use omicron_common::api::external::ByteCount;
    use omicron_common::api::external::IdentityMetadataCreateParams;
    use omicron_common::api::external::Instance;
    use omicron_common::api::external::InstanceCpuCount;
    use omicron_common::api::external::Name;
    use omicron_common::api::external::NameOrId;
    use sled_agent_client::CrucibleOpts;
    use sled_agent_client::TestInterfaces as SledAgentTestInterfaces;
    use std::str::FromStr;

    type DiskTest<'a> =
        nexus_test_utils::resource_helpers::DiskTest<'a, crate::Server>;

    #[test]
    fn test_create_snapshot_from_disk_modify_request() {
        let disk = VolumeConstructionRequest::Volume {
            block_size: 512,
            id: Uuid::new_v4(),
            read_only_parent: Some(
                Box::new(VolumeConstructionRequest::Volume {
                    block_size: 512,
                    id: Uuid::new_v4(),
                    read_only_parent: Some(
                        Box::new(VolumeConstructionRequest::Url {
                            id: Uuid::new_v4(),
                            block_size: 512,
                            url: "http://[fd01:1122:3344:101::15]/crucible-tester-sparse.img".into(),
                        })
                    ),
                    sub_volumes: vec![
                        VolumeConstructionRequest::Region {
                            block_size: 512,
                            blocks_per_extent: 10,
                            extent_count: 20,
                            gen: 1,
                            opts: CrucibleOpts {
                                id: Uuid::new_v4(),
                                key: Some("tkBksPOA519q11jvLCCX5P8t8+kCX4ZNzr+QP8M+TSg=".into()),
                                lossy: false,
                                read_only: true,
                                target: vec![
                                    "[fd00:1122:3344:101::8]:19001".parse().unwrap(),
                                    "[fd00:1122:3344:101::7]:19001".parse().unwrap(),
                                    "[fd00:1122:3344:101::6]:19001".parse().unwrap(),
                                ],
                                cert_pem: None,
                                key_pem: None,
                                root_cert_pem: None,
                                flush_timeout: None,
                                control: None,
                            }
                        },
                    ]
                }),
            ),
            sub_volumes: vec![
                VolumeConstructionRequest::Region {
                    block_size: 512,
                    blocks_per_extent: 10,
                    extent_count: 80,
                    gen: 100,
                    opts: CrucibleOpts {
                        id: Uuid::new_v4(),
                        key: Some("jVex5Zfm+avnFMyezI6nCVPRPs53EWwYMN844XETDBM=".into()),
                        lossy: false,
                        read_only: false,
                        target: vec![
                            "[fd00:1122:3344:101::8]:19002".parse().unwrap(),
                            "[fd00:1122:3344:101::7]:19002".parse().unwrap(),
                            "[fd00:1122:3344:101::6]:19002".parse().unwrap(),
                        ],
                        cert_pem: None,
                        key_pem: None,
                        root_cert_pem: None,
                        flush_timeout: None,
                        control: Some("127.0.0.1:12345".parse().unwrap()),
                    }
                },
            ],
        };

        let mut replace_sockets = ReplaceSocketsMap::new();

        // Replacements for top level Region only
        replace_sockets.insert(
            "[fd00:1122:3344:101::6]:19002".parse().unwrap(),
            "[fd01:1122:3344:101::6]:9000".parse().unwrap(),
        );
        replace_sockets.insert(
            "[fd00:1122:3344:101::7]:19002".parse().unwrap(),
            "[fd01:1122:3344:101::7]:9000".parse().unwrap(),
        );
        replace_sockets.insert(
            "[fd00:1122:3344:101::8]:19002".parse().unwrap(),
            "[fd01:1122:3344:101::8]:9000".parse().unwrap(),
        );

        let snapshot =
            create_snapshot_from_disk(&disk, &replace_sockets).unwrap();

        eprintln!("{:?}", serde_json::to_string(&snapshot).unwrap());

        // validate that each ID changed

        let snapshot_id_1 = match &snapshot {
            VolumeConstructionRequest::Volume { id, .. } => id,
            _ => panic!("enum changed shape!"),
        };

        let disk_id_1 = match &disk {
            VolumeConstructionRequest::Volume { id, .. } => id,
            _ => panic!("enum changed shape!"),
        };

        assert_ne!(snapshot_id_1, disk_id_1);

        let snapshot_first_opts = match &snapshot {
            VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                match &sub_volumes[0] {
                    VolumeConstructionRequest::Region { opts, .. } => opts,
                    _ => panic!("enum changed shape!"),
                }
            }
            _ => panic!("enum changed shape!"),
        };

        let disk_first_opts = match &disk {
            VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                match &sub_volumes[0] {
                    VolumeConstructionRequest::Region { opts, .. } => opts,
                    _ => panic!("enum changed shape!"),
                }
            }
            _ => panic!("enum changed shape!"),
        };

        assert_ne!(snapshot_first_opts.id, disk_first_opts.id);

        let snapshot_id_3 = match &snapshot {
            VolumeConstructionRequest::Volume { read_only_parent, .. } => {
                match read_only_parent.as_ref().unwrap().as_ref() {
                    VolumeConstructionRequest::Volume { id, .. } => id,
                    _ => panic!("enum changed shape!"),
                }
            }
            _ => panic!("enum changed shape!"),
        };

        let disk_id_3 = match &disk {
            VolumeConstructionRequest::Volume { read_only_parent, .. } => {
                match read_only_parent.as_ref().unwrap().as_ref() {
                    VolumeConstructionRequest::Volume { id, .. } => id,
                    _ => panic!("enum changed shape!"),
                }
            }
            _ => panic!("enum changed shape!"),
        };

        assert_ne!(snapshot_id_3, disk_id_3);

        let snapshot_second_opts = match &snapshot {
            VolumeConstructionRequest::Volume { read_only_parent, .. } => {
                match read_only_parent.as_ref().unwrap().as_ref() {
                    VolumeConstructionRequest::Volume {
                        sub_volumes, ..
                    } => match &sub_volumes[0] {
                        VolumeConstructionRequest::Region { opts, .. } => opts,
                        _ => panic!("enum changed shape!"),
                    },
                    _ => panic!("enum changed shape!"),
                }
            }
            _ => panic!("enum changed shape!"),
        };

        let disk_second_opts = match &disk {
            VolumeConstructionRequest::Volume { read_only_parent, .. } => {
                match read_only_parent.as_ref().unwrap().as_ref() {
                    VolumeConstructionRequest::Volume {
                        sub_volumes, ..
                    } => match &sub_volumes[0] {
                        VolumeConstructionRequest::Region { opts, .. } => opts,
                        _ => panic!("enum changed shape!"),
                    },
                    _ => panic!("enum changed shape!"),
                }
            }
            _ => panic!("enum changed shape!"),
        };

        assert_ne!(snapshot_second_opts.id, disk_second_opts.id);

        // validate only the top level targets were changed

        assert_ne!(snapshot_first_opts.target, disk_first_opts.target);
        assert_eq!(snapshot_second_opts.target, disk_second_opts.target);

        // validate that read_only was set correctly

        assert_eq!(snapshot_first_opts.read_only, true);
        assert_eq!(disk_first_opts.read_only, false);

        // validate control socket was removed

        assert!(snapshot_first_opts.control.is_none());
        assert!(disk_first_opts.control.is_some());
    }

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    const PROJECT_NAME: &str = "springfield-squidport";
    const DISK_NAME: &str = "disky-mcdiskface";
    const INSTANCE_NAME: &str = "base-instance";

    async fn create_project_and_disk_and_pool(
        client: &ClientTestContext,
    ) -> Uuid {
        create_default_ip_pool(&client).await;
        create_project(client, PROJECT_NAME).await;
        create_disk(client, PROJECT_NAME, DISK_NAME).await.identity.id
    }

    // Helper for creating snapshot create parameters
    fn new_test_params(
        opctx: &OpContext,
        silo_id: Uuid,
        project_id: Uuid,
        disk_id: Uuid,
        disk: NameOrId,
        attach_instance_id: Option<Uuid>,
        use_the_pantry: bool,
    ) -> Params {
        Params {
            serialized_authn: authn::saga::Serialized::for_opctx(opctx),
            silo_id,
            project_id,
            disk_id,
            attach_instance_id,
            use_the_pantry,
            create_params: params::SnapshotCreate {
                identity: IdentityMetadataCreateParams {
                    name: "my-snapshot".parse().expect("Invalid disk name"),
                    description: "My snapshot".to_string(),
                },
                disk,
            },
        }
    }

    pub fn test_opctx(cptestctx: &ControlPlaneTestContext) -> OpContext {
        OpContext::for_tests(
            cptestctx.logctx.log.new(o!()),
            cptestctx.server.server_context().nexus.datastore().clone(),
        )
    }

    #[nexus_test(server = crate::Server)]
    async fn test_saga_basic_usage_succeeds(
        cptestctx: &ControlPlaneTestContext,
    ) {
        // Basic snapshot test, create a snapshot of a disk that
        // is not attached to an instance.
        DiskTest::new(cptestctx).await;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let disk_id = create_project_and_disk_and_pool(&client).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(cptestctx);

        let (authz_silo, authz_project, _authz_disk) =
            LookupPath::new(&opctx, nexus.datastore())
                .disk_id(disk_id)
                .lookup_for(authz::Action::Read)
                .await
                .expect("Failed to look up created disk");

        let silo_id = authz_silo.id();
        let project_id = authz_project.id();

        let params = new_test_params(
            &opctx,
            silo_id,
            project_id,
            disk_id,
            Name::from_str(DISK_NAME).unwrap().into(),
            None, // not attached to an instance
            true, // use the pantry
        );
        // Actually run the saga
        let output = nexus
            .sagas
            .saga_execute::<SagaSnapshotCreate>(params)
            .await
            .unwrap();

        let snapshot = output
            .lookup_node_output::<nexus_db_queries::db::model::Snapshot>(
                "finalized_snapshot",
            )
            .unwrap();
        assert_eq!(snapshot.project_id, project_id);
    }

    async fn no_snapshot_records_exist(datastore: &DataStore) -> bool {
        use nexus_db_queries::db::model::Snapshot;
        use nexus_db_queries::db::schema::snapshot::dsl;

        dsl::snapshot
            .filter(dsl::time_deleted.is_null())
            .select(Snapshot::as_select())
            .first_async::<Snapshot>(
                &*datastore.pool_connection_for_tests().await.unwrap(),
            )
            .await
            .optional()
            .unwrap()
            .is_none()
    }

    async fn no_region_snapshot_records_exist(datastore: &DataStore) -> bool {
        use nexus_db_queries::db::model::RegionSnapshot;
        use nexus_db_queries::db::schema::region_snapshot::dsl;

        dsl::region_snapshot
            .select(RegionSnapshot::as_select())
            .first_async::<RegionSnapshot>(
                &*datastore.pool_connection_for_tests().await.unwrap(),
            )
            .await
            .optional()
            .unwrap()
            .is_none()
    }

    async fn verify_clean_slate(
        cptestctx: &ControlPlaneTestContext,
        test: &DiskTest<'_>,
    ) {
        // Verifies:
        // - No disk records exist
        // - No volume records exist
        // - No region allocations exist
        // - No regions are ensured in the sled agent
        crate::app::sagas::disk_create::test::verify_clean_slate(
            cptestctx, test,
        )
        .await;

        // Verifies:
        // - No snapshot records exist
        // - No region snapshot records exist
        let datastore = cptestctx.server.server_context().nexus.datastore();
        assert!(no_snapshot_records_exist(datastore).await);
        assert!(no_region_snapshot_records_exist(datastore).await);
    }

    /// Creates an instance in the test project with the supplied disks attached
    /// and ensures the instance is started.
    async fn setup_test_instance(
        cptestctx: &ControlPlaneTestContext,
        client: &ClientTestContext,
        disks_to_attach: Vec<InstanceDiskAttachment>,
    ) -> InstanceAndActiveVmm {
        let instances_url = format!("/v1/instances?project={}", PROJECT_NAME,);

        let mut disks_iter = disks_to_attach.into_iter();
        let boot_disk = disks_iter.next();
        let data_disks: Vec<InstanceDiskAttachment> = disks_iter.collect();

        let instance: Instance = object_create(
            client,
            &instances_url,
            &params::InstanceCreate {
                identity: IdentityMetadataCreateParams {
                    name: INSTANCE_NAME.parse().unwrap(),
                    description: format!("instance {:?}", INSTANCE_NAME),
                },
                ncpus: InstanceCpuCount(2),
                memory: ByteCount::from_gibibytes_u32(1),
                hostname: "base-instance".parse().unwrap(),
                user_data:
                    b"#cloud-config\nsystem_info:\n  default_user:\n    name: oxide"
                        .to_vec(),
                ssh_public_keys:  Some(Vec::new()),
                network_interfaces:
                    params::InstanceNetworkInterfaceAttachment::None,
                boot_disk,
                disks: data_disks,
                external_ips: vec![],
                start: true,
                auto_restart_policy: Default::default(),
            },
        )
        .await;

        // Read out the instance's assigned sled, then poke the instance to get
        // it from the Starting state to the Running state so the test disk can
        // be snapshotted.
        let nexus = &cptestctx.server.server_context().nexus;
        let opctx = test_opctx(&cptestctx);
        let (.., authz_instance) = LookupPath::new(&opctx, nexus.datastore())
            .instance_id(instance.identity.id)
            .lookup_for(authz::Action::Read)
            .await
            .unwrap();

        let instance_state = nexus
            .datastore()
            .instance_fetch_with_vmm(&opctx, &authz_instance)
            .await
            .unwrap();

        let vmm_state = instance_state
            .vmm()
            .as_ref()
            .expect("starting instance should have a vmm");
        let propolis_id = PropolisUuid::from_untyped_uuid(vmm_state.id);
        let sled_id = SledUuid::from_untyped_uuid(vmm_state.sled_id);
        let sa = nexus.sled_client(&sled_id).await.unwrap();
        sa.vmm_finish_transition(propolis_id).await;

        let instance_state = nexus
            .datastore()
            .instance_fetch_with_vmm(&opctx, &authz_instance)
            .await
            .unwrap();

        let new_state = instance_state
            .vmm()
            .as_ref()
            .expect("running instance should have a sled")
            .runtime
            .state;

        assert_eq!(new_state, nexus_db_model::VmmState::Running);

        instance_state
    }

    #[nexus_test(server = crate::Server)]
    async fn test_action_failure_can_unwind_no_pantry(
        cptestctx: &ControlPlaneTestContext,
    ) {
        test_action_failure_can_unwind_wrapper(cptestctx, false).await
    }

    #[nexus_test(server = crate::Server)]
    async fn test_action_failure_can_unwind_pantry(
        cptestctx: &ControlPlaneTestContext,
    ) {
        test_action_failure_can_unwind_wrapper(cptestctx, true).await
    }

    async fn test_action_failure_can_unwind_wrapper(
        cptestctx: &ControlPlaneTestContext,
        use_the_pantry: bool,
    ) {
        let test = DiskTest::new(cptestctx).await;
        let log = &cptestctx.logctx.log;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let disk_id = create_project_and_disk_and_pool(&client).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(&cptestctx);
        let (authz_silo, authz_project, _authz_disk) =
            LookupPath::new(&opctx, nexus.datastore())
                .disk_id(disk_id)
                .lookup_for(authz::Action::Read)
                .await
                .expect("Failed to look up created disk");

        let silo_id = authz_silo.id();
        let project_id = authz_project.id();

        // As a concession to the test helper, make sure the disk is gone
        // before the first attempt to run the saga recreates it.
        delete_disk(client, PROJECT_NAME, DISK_NAME).await;

        crate::app::sagas::test_helpers::action_failure_can_unwind::<
            SagaSnapshotCreate,
            _,
            _,
        >(
            nexus,
            || {
                Box::pin({
                    async {
                        let disk_id =
                            create_disk(client, PROJECT_NAME, DISK_NAME)
                                .await
                                .identity
                                .id;

                        // If the pantry isn't being used, make sure the disk is
                        // attached. Note that under normal circumstances, a
                        // disk can only be attached to a stopped instance, but
                        // since this is just a test, bypass the normal
                        // attachment machinery and just update the disk's
                        // database record directly.
                        let attach_instance_id = if !use_the_pantry {
                            let state = setup_test_instance(
                                cptestctx,
                                client,
                                vec![params::InstanceDiskAttachment::Attach(
                                    params::InstanceDiskAttach {
                                        name: Name::from_str(DISK_NAME)
                                            .unwrap(),
                                    },
                                )],
                            )
                            .await;

                            Some(state.instance().id())
                        } else {
                            None
                        };

                        new_test_params(
                            &opctx,
                            silo_id,
                            project_id,
                            disk_id,
                            Name::from_str(DISK_NAME).unwrap().into(),
                            attach_instance_id,
                            use_the_pantry,
                        )
                    }
                })
            },
            || {
                Box::pin(async {
                    // If the pantry wasn't used, detach the disk before
                    // deleting it. Note that because each iteration creates a
                    // new disk ID, and that ID doesn't escape the closure that
                    // created it, the lookup needs to be done by name instead.
                    if !use_the_pantry {
                        let (.., authz_disk, db_disk) =
                            LookupPath::new(&opctx, nexus.datastore())
                                .project_id(project_id)
                                .disk_name(&db::model::Name(
                                    DISK_NAME.to_owned().try_into().unwrap(),
                                ))
                                .fetch_for(authz::Action::Read)
                                .await
                                .expect("Failed to look up created disk");

                        assert!(nexus
                            .datastore()
                            .disk_update_runtime(
                                &opctx,
                                &authz_disk,
                                &db_disk.runtime().detach(),
                            )
                            .await
                            .expect("failed to detach disk"));

                        // Stop and destroy the test instance to satisfy the
                        // clean-slate check.
                        test_helpers::instance_stop_by_name(
                            cptestctx,
                            INSTANCE_NAME,
                            PROJECT_NAME,
                        )
                        .await;
                        test_helpers::instance_simulate_by_name(
                            cptestctx,
                            INSTANCE_NAME,
                            PROJECT_NAME,
                        )
                        .await;
                        // Wait until the instance has advanced to the `NoVmm`
                        // state before deleting it. This may not happen
                        // immediately, as the `Nexus::cpapi_instances_put` API
                        // endpoint simply writes the new VMM state to the
                        // database and *starts* an `instance-update` saga, and
                        // the instance record isn't updated until that saga
                        // completes.
                        test_helpers::instance_wait_for_state_by_name(
                            cptestctx,
                            INSTANCE_NAME,
                            PROJECT_NAME,
                            nexus_db_model::InstanceState::NoVmm,
                        )
                        .await;
                        test_helpers::instance_delete_by_name(
                            cptestctx,
                            INSTANCE_NAME,
                            PROJECT_NAME,
                        )
                        .await;
                    }

                    delete_disk(client, PROJECT_NAME, DISK_NAME).await;
                    verify_clean_slate(cptestctx, &test).await;
                })
            },
            log,
        )
        .await;
    }

    #[nexus_test(server = crate::Server)]
    async fn test_saga_use_the_pantry_wrongly_set(
        cptestctx: &ControlPlaneTestContext,
    ) {
        // Test the correct handling of the following race: the user requests a
        // snapshot of a disk that isn't attached to anything (meaning
        // `use_the_pantry` is true, but before the saga starts executing
        // something else attaches the disk to an instance.
        DiskTest::new(cptestctx).await;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let disk_id = create_project_and_disk_and_pool(&client).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(cptestctx);

        let (authz_silo, authz_project, _authz_disk) =
            LookupPath::new(&opctx, nexus.datastore())
                .disk_id(disk_id)
                .lookup_for(authz::Action::Read)
                .await
                .expect("Failed to look up created disk");

        let silo_id = authz_silo.id();
        let project_id = authz_project.id();

        let params = new_test_params(
            &opctx,
            silo_id,
            project_id,
            disk_id,
            Name::from_str(DISK_NAME).unwrap().into(),
            // The disk isn't attached at this time, so don't supply a sled.
            None,
            true, // use the pantry
        );

        let dag = create_saga_dag::<SagaSnapshotCreate>(params).unwrap();
        let runnable_saga = nexus.sagas.saga_prepare(dag).await.unwrap();

        // Before running the saga, attach the disk to an instance!
        let _instance_and_vmm = setup_test_instance(
            &cptestctx,
            &client,
            vec![params::InstanceDiskAttachment::Attach(
                params::InstanceDiskAttach {
                    name: Name::from_str(DISK_NAME).unwrap(),
                },
            )],
        )
        .await;

        // Actually run the saga
        let output = runnable_saga
            .run_to_completion()
            .await
            .unwrap()
            .into_omicron_result();

        // Expect to see 409
        match output {
            Err(e) => {
                assert!(matches!(e, Error::Conflict { .. }));
            }

            Ok(_) => {
                assert!(false);
            }
        }

        // Detach the disk, then rerun the saga
        let (.., authz_disk, db_disk) =
            LookupPath::new(&opctx, nexus.datastore())
                .disk_id(disk_id)
                .fetch_for(authz::Action::Read)
                .await
                .expect("Failed to look up created disk");

        assert!(nexus
            .datastore()
            .disk_update_runtime(
                &opctx,
                &authz_disk,
                &db_disk.runtime().detach(),
            )
            .await
            .expect("failed to detach disk"));

        // Rerun the saga
        let params = new_test_params(
            &opctx,
            silo_id,
            project_id,
            disk_id,
            Name::from_str(DISK_NAME).unwrap().into(),
            // The disk isn't attached at this time, so don't supply a sled.
            None,
            true, // use the pantry
        );

        let output =
            nexus.sagas.saga_execute::<SagaSnapshotCreate>(params).await;

        // Expect 200
        assert!(output.is_ok());
    }

    #[nexus_test(server = crate::Server)]
    async fn test_saga_use_the_pantry_wrongly_unset(
        cptestctx: &ControlPlaneTestContext,
    ) {
        // Test the correct handling of the following race: the user requests a
        // snapshot of a disk that is attached to an instance (meaning
        // `use_the_pantry` is false, but before the saga starts executing
        // something else detaches the disk.
        DiskTest::new(cptestctx).await;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let disk_id = create_project_and_disk_and_pool(&client).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(cptestctx);

        let (authz_silo, authz_project, _authz_disk) =
            LookupPath::new(&opctx, nexus.datastore())
                .disk_id(disk_id)
                .lookup_for(authz::Action::Read)
                .await
                .expect("Failed to look up created disk");

        let silo_id = authz_silo.id();
        let project_id = authz_project.id();

        // Synthesize an instance ID to pass to the saga, but use the default
        // test sled ID. This will direct a snapshot request to the simulated
        // sled agent specifying an instance it knows nothing about, which is
        // equivalent to creating an instance, attaching the test disk, creating
        // the saga, stopping the instance, detaching the disk, and then letting
        // the saga run.
        let fake_instance_id = Uuid::new_v4();

        let params = new_test_params(
            &opctx,
            silo_id,
            project_id,
            disk_id,
            Name::from_str(DISK_NAME).unwrap().into(),
            Some(fake_instance_id),
            false, // use the pantry
        );

        let dag = create_saga_dag::<SagaSnapshotCreate>(params).unwrap();
        let runnable_saga = nexus.sagas.saga_prepare(dag).await.unwrap();

        // Before running the saga, detach the disk!
        let (.., authz_disk, db_disk) =
            LookupPath::new(&opctx, nexus.datastore())
                .disk_id(disk_id)
                .fetch_for(authz::Action::Modify)
                .await
                .expect("Failed to look up created disk");

        assert!(nexus
            .datastore()
            .disk_update_runtime(
                &opctx,
                &authz_disk,
                &db_disk.runtime().detach(),
            )
            .await
            .expect("failed to detach disk"));

        // Actually run the saga. This should fail.
        let output = runnable_saga
            .run_to_completion()
            .await
            .unwrap()
            .into_omicron_result();

        assert!(output.is_err());

        // Attach the disk to an instance, then rerun the saga
        let instance_state = setup_test_instance(
            cptestctx,
            client,
            vec![params::InstanceDiskAttachment::Attach(
                params::InstanceDiskAttach {
                    name: Name::from_str(DISK_NAME).unwrap(),
                },
            )],
        )
        .await;

        // Rerun the saga
        let params = new_test_params(
            &opctx,
            silo_id,
            project_id,
            disk_id,
            Name::from_str(DISK_NAME).unwrap().into(),
            Some(instance_state.instance().id()),
            false, // use the pantry
        );

        let output =
            nexus.sagas.saga_execute::<SagaSnapshotCreate>(params).await;

        // Expect 200
        assert!(output.is_ok());
    }
}
*/
