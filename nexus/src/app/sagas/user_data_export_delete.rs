// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XXX TODO

use super::ActionRegistry;
use super::NexusActionContext;
use super::NexusSaga;
use super::common_storage::call_pantry_detach;
use crate::app::sagas::SagaInitError;
use crate::app::sagas::declare_saga_actions;
use crate::app::sagas::volume_delete;
use nexus_db_queries::authn;
use omicron_common::progenitor_operation_retry::ProgenitorOperationRetryError;
use omicron_uuid_kinds::GenericUuid;
use omicron_uuid_kinds::UserDataExportUuid;
use omicron_uuid_kinds::VolumeUuid;
use serde::Deserialize;
use serde::Serialize;
use slog_error_chain::InlineErrorChain;
use steno::ActionError;
use steno::Node;

// user data export delete saga: input parameters

#[derive(Debug, Deserialize, Serialize)]
pub struct Params {
    pub serialized_authn: authn::saga::Serialized,
    pub user_data_export_id: UserDataExportUuid,
    pub volume_id: VolumeUuid,
}

// user data export delete saga: actions

declare_saga_actions! {
    user_data_export_delete;
    CALL_PANTRY_DETACH_FOR_EXPORT -> "call_pantry_detach_for_export" {
        + suded_call_pantry_detach_for_export
    }
    DELETE_USER_DATA_EXPORT_RECORD -> "deleted_record" {
        + suded_delete_user_data_export_record
    }
}

// user data export delete saga: definition

#[derive(Debug)]
pub(crate) struct SagaUserDataExportDelete;
impl NexusSaga for SagaUserDataExportDelete {
    const NAME: &'static str = "user-data-export-delete";
    type Params = Params;

    fn register_actions(registry: &mut ActionRegistry) {
        user_data_export_delete_register_actions(registry);
    }

    fn make_saga_dag(
        params: &Self::Params,
        mut builder: steno::DagBuilder,
    ) -> Result<steno::Dag, super::SagaInitError> {
        builder.append(call_pantry_detach_for_export_action());

        let subsaga_params = volume_delete::Params {
            serialized_authn: params.serialized_authn.clone(),
            volume_id: params.volume_id,
        };

        let subsaga_dag = {
            let subsaga_builder = steno::DagBuilder::new(steno::SagaName::new(
                volume_delete::SagaVolumeDelete::NAME,
            ));
            volume_delete::SagaVolumeDelete::make_saga_dag(
                &subsaga_params,
                subsaga_builder,
            )?
        };

        builder.append(Node::constant(
            "params_for_volume_delete_subsaga",
            serde_json::to_value(&subsaga_params).map_err(|e| {
                SagaInitError::SerializeError(
                    "params_for_volume_delete_subsaga".to_string(),
                    e,
                )
            })?,
        ));

        builder.append(Node::subsaga(
            "volume_delete_subsaga_no_result",
            subsaga_dag,
            "params_for_volume_delete_subsaga",
        ));

        // Delete the user data export record last. There's no way to re-trigger
        // the delete once it is gone.
        builder.append(delete_user_data_export_record_action());

        Ok(builder.build()?)
    }
}

// user data export delete saga: action implementations

async fn suded_call_pantry_detach_for_export(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let log = sagactx.user_data().log();
    let params = sagactx.saga_params::<Params>()?;
    let osagactx = sagactx.user_data();
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let maybe_record = osagactx
        .datastore()
        .user_data_export_lookup_by_id(&opctx, params.user_data_export_id)
        .await
        .map_err(ActionError::action_failed)?;

    let Some(record) = maybe_record else {
        info!(
            log,
            "user data export {} hard deleted!", params.user_data_export_id,
        );
        return Err(ActionError::action_failed(String::from(
            "user data export hard deleted!",
        )));
    };

    let volume_id = record.volume_id();
    let pantry_address = record.pantry_address();

    info!(log, "detaching {volume_id} from pantry at {pantry_address}");

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

        Err(err) => Err(ActionError::action_failed(format!(
            "failed to detach {volume_id} from pantry at {pantry_address}: {}",
            InlineErrorChain::new(&err)
        ))),
    }
}

async fn suded_delete_user_data_export_record(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    osagactx
        .datastore()
        .user_data_export_delete(&opctx, params.user_data_export_id)
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::app::authz;
    use crate::app::saga::create_saga_dag;
    use nexus_db_lookup::LookupPath;
    use uuid::Uuid;

    use nexus_db_model::UserDataExportRecord;

    use nexus_db_queries::context::OpContext;

    use nexus_test_utils::background::run_user_data_export_coordinator;
    use nexus_test_utils::resource_helpers::create_default_ip_pool;
    use nexus_test_utils::resource_helpers::create_disk;
    use nexus_test_utils::resource_helpers::create_project;
    use nexus_test_utils::resource_helpers::create_snapshot;

    use nexus_test_utils_macros::nexus_test;

    use omicron_common::api::external::Error;

    use omicron_test_utils::dev::poll;

    use std::time::Duration;

    type DiskTest<'a> =
        nexus_test_utils::resource_helpers::DiskTest<'a, crate::Server>;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    const PROJECT_NAME: &str = "bobs-barrel-of-bits";
    const DISK_NAME: &str = "bobs-disk";
    const SNAPSHOT_NAME: &str = "bobs-snapshot";

    async fn create_all_the_stuff(
        cptestctx: &ControlPlaneTestContext,
    ) -> (Uuid, UserDataExportRecord) {
        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;

        create_default_ip_pool(&client).await;
        create_project(client, PROJECT_NAME).await;
        create_disk(client, PROJECT_NAME, DISK_NAME).await;

        let snapshot_id =
            create_snapshot(client, PROJECT_NAME, DISK_NAME, SNAPSHOT_NAME)
                .await
                .identity
                .id;

        let opctx = test_opctx(cptestctx);

        // Run the background task to create the record
        run_user_data_export_coordinator(&cptestctx.internal_client).await;

        let (.., authz_snapshot) = LookupPath::new(&opctx, nexus.datastore())
            .snapshot_id(snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .unwrap();

        let export_object = poll::wait_for_condition(
            || {
                let opctx = test_opctx(cptestctx);
                let datastore = nexus.datastore().clone();
                let authz_snapshot = authz_snapshot.clone();

                async move {
                    let maybe_object = datastore
                        .user_data_export_lookup_for_snapshot(
                            &opctx,
                            &authz_snapshot,
                        )
                        .await
                        .unwrap();

                    match maybe_object {
                        Some(object) => Ok(object),

                        None => Err(poll::CondCheckError::<Error>::NotYet),
                    }
                }
            },
            &Duration::from_millis(10),
            &Duration::from_secs(20),
        )
        .await
        .unwrap();

        (snapshot_id, export_object)
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

        let nexus = &cptestctx.server.server_context().nexus;
        let (snapshot_id, export_object) =
            create_all_the_stuff(cptestctx).await;

        // Create the delete saga dag
        let opctx = test_opctx(cptestctx);

        let params = Params {
            serialized_authn: authn::saga::Serialized::for_opctx(&opctx),
            user_data_export_id: export_object.id(),
            volume_id: export_object.volume_id(),
        };

        nexus
            .sagas
            .saga_execute::<SagaUserDataExportDelete>(params)
            .await
            .unwrap();

        // Make sure the record was deleted ok
        let (.., authz_snapshot) = LookupPath::new(&opctx, nexus.datastore())
            .snapshot_id(snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .unwrap();

        let export_object = nexus
            .datastore()
            .user_data_export_lookup_for_snapshot(&opctx, &authz_snapshot)
            .await
            .unwrap();

        assert!(export_object.is_none());
    }

    #[nexus_test(server = crate::Server)]
    async fn test_actions_succeed_idempotently(
        cptestctx: &ControlPlaneTestContext,
    ) {
        DiskTest::new(cptestctx).await;

        let nexus = &cptestctx.server.server_context().nexus;
        let (snapshot_id, export_object) =
            create_all_the_stuff(cptestctx).await;

        // Create the delete saga dag
        let opctx = test_opctx(cptestctx);

        let params = Params {
            serialized_authn: authn::saga::Serialized::for_opctx(&opctx),
            user_data_export_id: export_object.id(),
            volume_id: export_object.volume_id(),
        };

        let dag = create_saga_dag::<SagaUserDataExportDelete>(params).unwrap();

        crate::app::sagas::test_helpers::actions_succeed_idempotently(
            nexus, dag,
        )
        .await;

        // Make sure the record was deleted ok
        let (.., authz_snapshot) = LookupPath::new(&opctx, nexus.datastore())
            .snapshot_id(snapshot_id)
            .lookup_for(authz::Action::Read)
            .await
            .unwrap();

        let export_object = nexus
            .datastore()
            .user_data_export_lookup_for_snapshot(&opctx, &authz_snapshot)
            .await
            .unwrap();

        assert!(export_object.is_none());
    }
}
