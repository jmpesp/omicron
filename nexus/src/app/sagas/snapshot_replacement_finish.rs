// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compared to the rest of the snapshot replacement process, finishing the
//! process is straight forward. This saga is responsible for the following
//! snapshot replacement request state transitions:
//!
//! ```text
//!     ReplacementDone  <--
//!                         |
//!            |            |
//!            v            |
//!                         |
//!        Completing     --
//!
//!            |
//!            v
//!
//!        Completed
//! ```
//!
//! It will set itself as the "operating saga" for a snapshot replacement
//! request, change the state to "Completing", and:
//!
//! 1. Call the Volume delete saga for the fake Volume that points to the old
//!    snapshot.
//!
//! 2. Clear the operating saga id from the request record, and change the state
//!    to Completed.

use super::{
    ActionRegistry, NexusActionContext, NexusSaga, SagaInitError,
    ACTION_GENERATE_ID,
};
use crate::app::sagas::declare_saga_actions;
use crate::app::sagas::volume_delete;
use crate::app::{authn, db};
use serde::Deserialize;
use serde::Serialize;
use steno::ActionError;
use steno::Node;
use uuid::Uuid;

// snapshot replacement finish saga: input parameters

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Params {
    pub serialized_authn: authn::saga::Serialized,
    /// The fake volume created for the snapshot that was replaced
    // Note: this is only required in the params to build the volume-delete sub
    // saga
    pub old_snapshot_volume_id: Uuid,
    pub request: db::model::SnapshotReplacement,
}

// snapshot replacement finish saga: actions

declare_saga_actions! {
    snapshot_replacement_finish;
    SET_SAGA_ID -> "unused_1" {
        + srfs_set_saga_id
        - srfs_set_saga_id_undo
    }
    UPDATE_REQUEST_RECORD -> "unused_2" {
        + srfs_update_request_record
    }
}

// snapshot replacement finish saga: definition

#[derive(Debug)]
pub(crate) struct SagaSnapshotReplacementFinish;
impl NexusSaga for SagaSnapshotReplacementFinish {
    const NAME: &'static str = "snapshot-replacement-finish";
    type Params = Params;

    fn register_actions(registry: &mut ActionRegistry) {
        snapshot_replacement_finish_register_actions(registry);
    }

    fn make_saga_dag(
        params: &Self::Params,
        mut builder: steno::DagBuilder,
    ) -> Result<steno::Dag, SagaInitError> {
        builder.append(Node::action(
            "saga_id",
            "GenerateSagaId",
            ACTION_GENERATE_ID.as_ref(),
        ));

        builder.append(set_saga_id_action());

        let subsaga_params = volume_delete::Params {
            serialized_authn: params.serialized_authn.clone(),
            volume_id: params.old_snapshot_volume_id,
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

        builder.append(update_request_record_action());

        Ok(builder.build()?)
    }
}

// snapshot replacement finish saga: action implementations

async fn srfs_set_saga_id(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;

    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let saga_id = sagactx.lookup::<Uuid>("saga_id")?;

    // Change the request record here to an intermediate "completing" state to
    // block out other sagas that will be triggered for the same request.

    osagactx
        .datastore()
        .set_snapshot_replacement_completing(&opctx, params.request.id, saga_id)
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

async fn srfs_set_saga_id_undo(
    sagactx: NexusActionContext,
) -> Result<(), anyhow::Error> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let saga_id = sagactx.lookup::<Uuid>("saga_id")?;

    osagactx
        .datastore()
        .undo_set_snapshot_replacement_completing(
            &opctx,
            params.request.id,
            saga_id,
        )
        .await?;

    Ok(())
}

async fn srfs_update_request_record(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let params = sagactx.saga_params::<Params>()?;
    let osagactx = sagactx.user_data();
    let datastore = osagactx.datastore();
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let saga_id = sagactx.lookup::<Uuid>("saga_id")?;

    // Now that the snapshot has been deleted, update the replacement request
    // record to 'Complete' and clear the operating saga id. There is no undo
    // step for this, it should succeed idempotently.
    datastore
        .set_snapshot_replacement_complete(&opctx, params.request.id, saga_id)
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

#[cfg(test)]
pub(crate) mod test {
    use crate::{
        app::sagas::snapshot_replacement_finish::Params,
        app::sagas::snapshot_replacement_finish::SagaSnapshotReplacementFinish,
    };
    use async_bb8_diesel::AsyncRunQueryDsl;
    use chrono::Utc;
    use nexus_db_model::SnapshotReplacement;
    use nexus_db_model::SnapshotReplacementState;
    use nexus_db_model::Volume;
    use nexus_db_model::VolumeRepair;
    use nexus_db_queries::authn::saga::Serialized;
    use nexus_db_queries::context::OpContext;
    use nexus_test_utils_macros::nexus_test;
    use sled_agent_client::types::CrucibleOpts;
    use sled_agent_client::types::VolumeConstructionRequest;
    use uuid::Uuid;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    #[nexus_test(server = crate::Server)]
    async fn test_snapshot_replacement_finish_saga(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        // Manually insert required records
        let old_snapshot_volume_id = Uuid::new_v4();

        let volume_construction_request = VolumeConstructionRequest::Volume {
            id: old_snapshot_volume_id,
            block_size: 0,
            sub_volumes: vec![VolumeConstructionRequest::Region {
                block_size: 0,
                blocks_per_extent: 0,
                extent_count: 0,
                gen: 0,
                opts: CrucibleOpts {
                    id: old_snapshot_volume_id,
                    target: vec![
                        // XXX if you put something here, you'll need a
                        // synthetic dataset record
                    ],
                    lossy: false,
                    flush_timeout: None,
                    key: None,
                    cert_pem: None,
                    key_pem: None,
                    root_cert_pem: None,
                    control: None,
                    read_only: false,
                },
            }],
            read_only_parent: None,
        };

        let volume_data =
            serde_json::to_string(&volume_construction_request).unwrap();

        datastore
            .volume_create(Volume::new(old_snapshot_volume_id, volume_data))
            .await
            .unwrap();

        let request = SnapshotReplacement {
            id: Uuid::new_v4(),
            request_time: Utc::now(),
            old_dataset_id: Uuid::new_v4(),
            old_region_id: Uuid::new_v4(),
            old_snapshot_id: Uuid::new_v4(),
            old_snapshot_volume_id: Some(old_snapshot_volume_id),
            new_region_id: None,
            replacement_state: SnapshotReplacementState::ReplacementDone,
            operating_saga_id: None,
        };

        datastore
            .insert_snapshot_replacement_request_with_volume_id(
                &opctx,
                request.clone(),
                Uuid::new_v4(),
            )
            .await
            .unwrap();

        // Run the region replacement finish saga
        let params = Params {
            serialized_authn: Serialized::for_opctx(&opctx),
            old_snapshot_volume_id,
            request: request.clone(),
        };

        let _output = nexus
            .sagas
            .saga_execute::<SagaSnapshotReplacementFinish>(params)
            .await
            .unwrap();

        // Validate the state transition
        let result = datastore
            .get_snapshot_replacement_request_by_id(&opctx, request.id)
            .await
            .unwrap();
        assert_eq!(
            result.replacement_state,
            SnapshotReplacementState::Complete
        );
        assert!(result.operating_saga_id.is_none());

        // Validate the Volume was deleted
        assert!(datastore
            .volume_get(old_snapshot_volume_id)
            .await
            .unwrap()
            .is_none());

        // Validate that the associated VolumeRepair record was deleted
        let volume_repair_records: Vec<VolumeRepair> = {
            use async_bb8_diesel::AsyncSimpleConnection;
            use diesel::prelude::*;
            use nexus_db_queries::db::schema::volume_repair::dsl;

            let conn = datastore.pool_connection_for_tests().await.unwrap();

            datastore
                .transaction_retry_wrapper("check_for_volume_repair_records")
                .transaction(&conn, |conn| async move {
                    conn.batch_execute_async(
                        nexus_test_utils::db::ALLOW_FULL_TABLE_SCAN_SQL,
                    )
                    .await
                    .unwrap();

                    Ok(dsl::volume_repair
                        .filter(dsl::repair_id.eq(request.id))
                        .select(VolumeRepair::as_select())
                        .get_results_async::<VolumeRepair>(&conn)
                        .await
                        .unwrap())
                })
                .await
                .unwrap()
        };

        assert!(volume_repair_records.is_empty());
    }
}
