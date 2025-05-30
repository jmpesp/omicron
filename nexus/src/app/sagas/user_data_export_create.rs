// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XXX TODO

use super::{
    ACTION_GENERATE_ID, ActionRegistry, NexusActionContext, NexusSaga,
    SagaInitError,
    common_storage::{
        call_pantry_attach_for_volume, call_pantry_detach, get_pantry_address,
        is_pantry_gone,
    },
};
use crate::app::sagas::declare_saga_actions;
use crate::app::{authn, authz, db};
use crate::external_api::params;
use anyhow::anyhow;
use nexus_db_lookup::LookupPath;
use nexus_db_model::Generation;
use nexus_db_model::UserDataExportResource;
use nexus_db_queries::db::identity::{Asset, Resource};
use omicron_common::api::external::Error;
use omicron_common::progenitor_operation_retry::ProgenitorOperationRetryError;
use omicron_common::{
    api::external, progenitor_operation_retry::ProgenitorOperationRetry,
};
use omicron_uuid_kinds::GenericUuid;
use omicron_uuid_kinds::PropolisUuid;
use omicron_uuid_kinds::SledUuid;
use omicron_uuid_kinds::UserDataExportUuid;
use omicron_uuid_kinds::VolumeUuid;
use rand::{RngCore, SeedableRng, rngs::StdRng};
use serde::Deserialize;
use serde::Serialize;
use sled_agent_client::CrucibleOpts;
use sled_agent_client::VolumeConstructionRequest;
use sled_agent_client::types::VmmIssueDiskSnapshotRequestBody;
use slog::info;
use slog_error_chain::InlineErrorChain;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::net::SocketAddrV6;
use steno::ActionError;
use steno::Node;
use uuid::Uuid;

// user data export create saga: input parameters

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Params {
    pub serialized_authn: authn::saga::Serialized,
    pub resource: UserDataExportResource,
}

// user data export create saga: actions
declare_saga_actions! {
    user_data_export_create;
    CREATE_EXPORT_VOLUME -> "export_volume" {
        + sudec_create_export_volume
        - sudec_create_export_volume_undo
    }
    GET_PANTRY_ADDRESS -> "pantry_address" {
        + sudec_get_pantry_address
    }
    CALL_PANTRY_ATTACH_FOR_EXPORT -> "call_pantry_attach_for_export" {
        + sudec_call_pantry_attach_for_export
        - sudec_call_pantry_attach_for_export_undo
    }
    CREATE_EXPORT_RECORD -> "created_record" {
        + sudec_create_export_record
        - sudec_create_export_record_undo
    }
}

// user data export create saga: definition

#[derive(Debug)]
pub(crate) struct SagaUserDataExportCreate;
impl NexusSaga for SagaUserDataExportCreate {
    const NAME: &'static str = "user-data-export-create";
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

// user data export saga: action implementations

async fn sudec_create_export_volume(
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

    let source_volume_id = match params.resource {
        UserDataExportResource::Snapshot { id } => {
            debug!(log, "grabbing snapshot {id}");

            let (.., db_snapshot) =
                LookupPath::new(&opctx, osagactx.datastore())
                    .snapshot_id(id)
                    .fetch()
                    .await
                    .map_err(ActionError::action_failed)?;

            debug!(
                log,
                "copying snapshot {} volume {} to {volume_id}",
                db_snapshot.id(),
                db_snapshot.volume_id(),
            );

            db_snapshot.volume_id()
        }

        UserDataExportResource::Image { id } => {
            debug!(log, "grabbing image {id}");

            let (.., db_image) = LookupPath::new(&opctx, osagactx.datastore())
                .image_id(id)
                .fetch()
                .await
                .map_err(ActionError::action_failed)?;

            debug!(
                log,
                "copying image {} volume {} to {volume_id}",
                db_image.id(),
                db_image.volume_id(),
            );

            db_image.volume_id()
        }
    };

    osagactx
        .datastore()
        .volume_checkout_randomize_ids(
            db::datastore::SourceVolume(source_volume_id),
            db::datastore::DestVolume(volume_id),
            db::datastore::VolumeCheckoutReason::ReadOnlyCopy,
        )
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

async fn sudec_create_export_volume_undo(
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

async fn sudec_get_pantry_address(
    sagactx: NexusActionContext,
) -> Result<SocketAddrV6, ActionError> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;

    let pantry_address = get_pantry_address(osagactx.nexus()).await?;

    info!(log, "using pantry at {}", pantry_address);

    Ok(pantry_address)
}

async fn sudec_call_pantry_attach_for_export(
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
                &format!("volume {volume_id} gone!"),
            )));
        }

        Err(e) => {
            return Err(ActionError::action_failed(Error::internal_error(
                &format!("failed to get volume {volume_id}: {e}"),
            )));
        }
    };

    let volume_construction_request = serde_json::from_str(&volume.data())
        .map_err(|e| {
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

async fn sudec_call_pantry_attach_for_export_undo(
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
        )),
    }
}

async fn sudec_create_export_record(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;

    let volume_id = sagactx.lookup::<VolumeUuid>("volume_id")?;
    let pantry_address = sagactx.lookup::<SocketAddrV6>("pantry_address")?;
    let user_data_export_id =
        sagactx.lookup::<UserDataExportUuid>("user_data_export_id")?;

    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let user_data_export = match params.resource {
        UserDataExportResource::Snapshot { id } => {
            let (.., authz_snapshot) =
                LookupPath::new(&opctx, osagactx.datastore())
                    .snapshot_id(id)
                    .lookup_for(authz::Action::Read)
                    .await
                    .map_err(ActionError::action_failed)?;

            osagactx
                .datastore()
                .user_data_export_create_for_snapshot(
                    &opctx,
                    user_data_export_id,
                    &authz_snapshot,
                    pantry_address,
                    volume_id,
                )
                .await
                .map_err(ActionError::action_failed)?
        }

        UserDataExportResource::Image { id: image_id } => {
            osagactx
                .datastore()
                .user_data_export_create_for_image(
                    &opctx,
                    user_data_export_id,
                    image_id,
                    pantry_address,
                    volume_id,
                )
                .await
                .map_err(ActionError::action_failed)?
        }
    };

    info!(log, "export {} created ok", user_data_export.id());

    Ok(())
}

async fn sudec_create_export_record_undo(
    sagactx: NexusActionContext,
) -> Result<(), anyhow::Error> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;

    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let user_data_export_id =
        sagactx.lookup::<UserDataExportUuid>("user_data_export_id")?;

    info!(log, "calling delete export {user_data_export_id}");

    osagactx
        .datastore()
        .user_data_export_delete(&opctx, user_data_export_id)
        .await?;

    Ok(())
}

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
    use nexus_db_queries::db::DataStore;
    use nexus_db_queries::db::datastore::InstanceAndActiveVmm;
    use nexus_test_utils::background::run_user_data_export_coordinator;
    use nexus_test_utils::resource_helpers::create_default_ip_pool;
    use nexus_test_utils::resource_helpers::create_disk;
    use nexus_test_utils::resource_helpers::create_project;
    use nexus_test_utils::resource_helpers::create_snapshot;
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
    use omicron_test_utils::dev::poll;
    use std::time::Duration;

    type DiskTest<'a> =
        nexus_test_utils::resource_helpers::DiskTest<'a, crate::Server>;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    const PROJECT_NAME: &str = "bobs-barrel-of-bits";
    const DISK_NAME: &str = "bobs-disk";
    const INSTANCE_NAME: &str = "bobs-instance";
    const SNAPSHOT_NAME: &str = "bobs-snapshot";

    async fn create_all_the_stuff(cptestctx: &ControlPlaneTestContext) -> Uuid {
        let client = &cptestctx.external_client;

        create_default_ip_pool(&client).await;
        create_project(client, PROJECT_NAME).await;
        create_disk(client, PROJECT_NAME, DISK_NAME).await;

        let snapshot_id = create_snapshot(
            client,
            PROJECT_NAME,
            DISK_NAME,
            SNAPSHOT_NAME,
        )
        .await
        .identity
        .id;

        // Trigger the background task to create the user data export object.
        // Wait for that to be created here, then delete it.

        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = test_opctx(cptestctx);

        let (.., authz_snapshot) = LookupPath::new(&opctx, nexus.datastore())
            .snapshot_id(snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .unwrap();

        run_user_data_export_coordinator(&cptestctx.internal_client).await;

        let export_object = poll::wait_for_condition(
            || {
                let opctx = test_opctx(cptestctx);
                let datastore = datastore.clone();
                let authz_snapshot = authz_snapshot.clone();

                async move {
                    let maybe_object = datastore
                        .user_data_export_lookup_for_snapshot(
                            &opctx, &authz_snapshot,
                        )
                        .await
                        .unwrap();

                    match maybe_object {
                        Some(object) => Ok(object),

                        None => {
                            Err(poll::CondCheckError::<Error>::NotYet)
                        }
                    }
                }
            },
            &Duration::from_millis(10),
            &Duration::from_secs(20),
        )
        .await
        .unwrap();

        datastore.user_data_export_delete(
            &opctx,
            export_object.id(),
        )
        .await
        .unwrap();

        snapshot_id
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
        DiskTest::new(cptestctx).await;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let snapshot_id = create_all_the_stuff(cptestctx).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(cptestctx);

        let params = Params {
            serialized_authn: authn::saga::Serialized::for_opctx(&opctx),
            resource: UserDataExportResource::Snapshot { id: snapshot_id },
        };

        // Actually run the saga
        nexus
            .sagas
            .saga_execute::<SagaUserDataExportCreate>(params)
            .await
            .unwrap();

        // Make sure the record was created ok
        let (.., authz_snapshot) = LookupPath::new(&opctx, nexus.datastore())
            .snapshot_id(snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .unwrap();

        assert!(
            nexus
                .datastore()
                .user_data_export_lookup_for_snapshot(&opctx, &authz_snapshot,)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[nexus_test(server = crate::Server)]
    async fn test_actions_succeed_idempotently(
        cptestctx: &ControlPlaneTestContext,
    ) {
        DiskTest::new(cptestctx).await;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let snapshot_id = create_all_the_stuff(cptestctx).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(cptestctx);

        let params = Params {
            serialized_authn: authn::saga::Serialized::for_opctx(&opctx),
            resource: UserDataExportResource::Snapshot { id: snapshot_id },
        };

        let dag = create_saga_dag::<SagaUserDataExportCreate>(params).unwrap();

        crate::app::sagas::test_helpers::actions_succeed_idempotently(
            nexus, dag,
        )
        .await;

        // Make sure the record was created ok
        let (.., authz_snapshot) = LookupPath::new(&opctx, nexus.datastore())
            .snapshot_id(snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .unwrap();

        assert!(
            nexus
                .datastore()
                .user_data_export_lookup_for_snapshot(&opctx, &authz_snapshot,)
                .await
                .unwrap()
                .is_some()
        );
    }

    async fn verify_clean_slate(
        cptestctx: &ControlPlaneTestContext,
        snapshot_id: Uuid,
    ) {
        let opctx = test_opctx(cptestctx);
        let nexus = &cptestctx.server.server_context().nexus;

        crate::app::sagas::test_helpers::assert_no_failed_undo_steps(
            &cptestctx.logctx.log,
            nexus.datastore(),
        )
        .await;

        let (.., authz_snapshot) = LookupPath::new(&opctx, nexus.datastore())
            .snapshot_id(snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .unwrap();

        // Validate no user data export record exists
        assert!(
            nexus
                .datastore()
                .user_data_export_lookup_for_snapshot(&opctx, &authz_snapshot)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[nexus_test(server = crate::Server)]
    async fn test_action_failure_can_unwind(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let test = DiskTest::new(cptestctx).await;
        let log = &cptestctx.logctx.log;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;

        let snapshot_id = create_all_the_stuff(cptestctx).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(&cptestctx);

        crate::app::sagas::test_helpers::action_failure_can_unwind::<
            SagaUserDataExportCreate,
            _,
            _,
        >(
            nexus,
            || {
                Box::pin(async {
                    Params {
                        serialized_authn: authn::saga::Serialized::for_opctx(
                            &opctx,
                        ),
                        resource: UserDataExportResource::Snapshot {
                            id: snapshot_id,
                        },
                    }
                })
            },
            || {
                Box::pin(async {
                    verify_clean_slate(cptestctx, snapshot_id).await;
                })
            },
            log,
        )
        .await;
    }

    #[nexus_test(server = crate::Server)]
    async fn test_action_failure_can_unwind_idempotently(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let test = DiskTest::new(cptestctx).await;
        let log = &cptestctx.logctx.log;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;

        let snapshot_id = create_all_the_stuff(cptestctx).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(&cptestctx);

        crate::app::sagas::test_helpers::action_failure_can_unwind_idempotently::<
            SagaUserDataExportCreate,
            _,
            _,
        >(
            nexus,
            || {
                Box::pin(async {
                    Params {
                        serialized_authn: authn::saga::Serialized::for_opctx(&opctx),
                        resource: UserDataExportResource::Snapshot {
                            id: snapshot_id,
                        },
                    }
                })
            },
            || {
                Box::pin(async {
                    verify_clean_slate(cptestctx, snapshot_id).await;
                })
            },
            log,
        )
        .await;
    }
}
