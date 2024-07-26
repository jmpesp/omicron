// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Clean up the volume that stashes the target replaced during the snapshot
//! replacement start saga. After that's done, change the snapshot replacement
//! state to Running. This saga handles the following snapshot replacement
//! request state transitions:
//!
//! ```text
//!    ReplacementDone  <--
//!                       |
//!          |            |
//!          v            |
//!                       |
//!  DeletingOldVolume  --
//!
//!          |
//!          v
//!
//!       Running
//! ```
//!
//! See the documentation for the "snapshot replacement step" saga for the next
//! step(s) in the process.

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

// snapshot replacement garbage collect saga: input parameters

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Params {
    pub serialized_authn: authn::saga::Serialized,
    /// The fake volume created for the snapshot that was replaced
    // Note: this is only required in the params to build the volume-delete sub
    // saga
    pub old_snapshot_volume_id: Uuid,
    pub request: db::model::SnapshotReplacement,
}

// snapshot replacement garbage collect saga: actions

declare_saga_actions! {
    snapshot_replacement_garbage_collect;
    SET_SAGA_ID -> "unused_1" {
        + srgs_set_saga_id
        - srgs_set_saga_id_undo
    }
    UPDATE_REQUEST_RECORD -> "unused_2" {
        + srgs_update_request_record
    }
}

// snapshot replacement garbage collect saga: definition

#[derive(Debug)]
pub(crate) struct SagaSnapshotReplacementGarbageCollect;
impl NexusSaga for SagaSnapshotReplacementGarbageCollect {
    const NAME: &'static str = "snapshot-replacement-garbage-collect";
    type Params = Params;

    fn register_actions(registry: &mut ActionRegistry) {
        snapshot_replacement_garbage_collect_register_actions(registry);
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

// snapshot replacement garbage collect saga: action implementations

async fn srgs_set_saga_id(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;

    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let saga_id = sagactx.lookup::<Uuid>("saga_id")?;

    // Change the request record here to an intermediate "deleting old volume"
    // state to block out other sagas that will be triggered for the same
    // request.
    osagactx
        .datastore()
        .set_snapshot_replacement_deleting_old_volume(
            &opctx,
            params.request.id,
            saga_id,
        )
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

async fn srgs_set_saga_id_undo(
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
        .undo_set_snapshot_replacement_deleting_old_volume(
            &opctx,
            params.request.id,
            saga_id,
        )
        .await?;

    Ok(())
}

async fn srgs_update_request_record(
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

    // Now that the snapshot volume has been deleted, update the replacement
    // request record to 'Running'. There is no undo step for this, it should
    // succeed idempotently.

    datastore
        .set_snapshot_replacement_running(&opctx, params.request.id, saga_id)
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

#[cfg(test)]
pub(crate) mod test {
    use crate::app::sagas::snapshot_replacement_garbage_collect::{
        Params, SagaSnapshotReplacementGarbageCollect,
    };
    use nexus_db_model::SnapshotReplacement;
    use nexus_db_model::SnapshotReplacementState;
    use nexus_db_model::Volume;
    use nexus_db_queries::authn::saga::Serialized;
    use nexus_db_queries::context::OpContext;
    use nexus_test_utils_macros::nexus_test;
    use sled_agent_client::types::CrucibleOpts;
    use sled_agent_client::types::VolumeConstructionRequest;
    use uuid::Uuid;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    #[nexus_test(server = crate::Server)]
    async fn test_snapshot_replacement_garbage_collect_saga(
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

        let mut request = SnapshotReplacement::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        request.replacement_state = SnapshotReplacementState::ReplacementDone;
        request.old_snapshot_volume_id = Some(old_snapshot_volume_id);

        datastore
            .insert_snapshot_replacement_request_with_volume_id(
                &opctx,
                request.clone(),
                Uuid::new_v4(),
            )
            .await
            .unwrap();

        // Run the saga
        let params = Params {
            serialized_authn: Serialized::for_opctx(&opctx),
            old_snapshot_volume_id,
            request: request.clone(),
        };

        let _output = nexus
            .sagas
            .saga_execute::<SagaSnapshotReplacementGarbageCollect>(params)
            .await
            .unwrap();

        // Validate the state transition
        let result = datastore
            .get_snapshot_replacement_request_by_id(&opctx, request.id)
            .await
            .unwrap();

        assert_eq!(result.replacement_state, SnapshotReplacementState::Running);

        // Validate the Volume was deleted
        assert!(datastore
            .volume_get(old_snapshot_volume_id)
            .await
            .unwrap()
            .is_none());
    }
}
