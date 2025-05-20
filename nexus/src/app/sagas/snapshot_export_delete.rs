// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XXX TODO

use crate::app::authz;
use super::ActionRegistry;
use super::NexusActionContext;
use super::NexusSaga;
use crate::app::sagas::declare_saga_actions;
use nexus_db_lookup::LookupPath;
use crate::app::sagas::volume_delete;
use crate::app::sagas::SagaInitError;
use nexus_db_queries::authn;
use nexus_db_queries::db;
use omicron_common::api::external::DiskState;
use omicron_uuid_kinds::VolumeUuid;
use omicron_uuid_kinds::SnapshotExportUuid;
use serde::Deserialize;
use serde::Serialize;
use steno::ActionError;
use steno::Node;
use uuid::Uuid;

// snapshot export delete saga: input parameters

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Params {
    pub serialized_authn: authn::saga::Serialized,
    pub snapshot_id: Uuid,
    pub snapshot_export_id: SnapshotExportUuid,
    pub volume_id: VolumeUuid,
}

// snapshot export delete saga: actions

declare_saga_actions! {
    snapshot_export_delete;
    PERM_CHECK -> "permission_check" {
        + sdd_delete_snapshot_export_perm_check
    }
    DELETE_SNAPSHOT_EXPORT_RECORD -> "deleted_record" {
        + sdd_delete_snapshot_export_record
    }
}

// snapshot export delete saga: definition

#[derive(Debug)]
pub(crate) struct SagaSnapshotExportDelete;
impl NexusSaga for SagaSnapshotExportDelete {
    const NAME: &'static str = "snapshot-export-delete";
    type Params = Params;

    fn register_actions(registry: &mut ActionRegistry) {
        snapshot_export_delete_register_actions(registry);
    }

    fn make_saga_dag(
        params: &Self::Params,
        mut builder: steno::DagBuilder,
    ) -> Result<steno::Dag, super::SagaInitError> {
        // Before deleting the volume, check first if the context has the
        // permission to delete the snapshot export.
        builder.append(perm_check_action());

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

        // Delete the snapshot export record last. There's no way to re-trigger
        // the delete once it is gone.
        builder.append(delete_snapshot_export_record_action());

        Ok(builder.build()?)
    }
}

// snapshot export delete saga: action implementations

async fn sdd_delete_snapshot_export_perm_check(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let log = sagactx.user_data().log();
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let (.., authz_snapshot) =
        LookupPath::new(&opctx, osagactx.datastore())
            .snapshot_id(params.snapshot_id)
            .lookup_for(authz::Action::Delete)
            .await
            .expect("Failed to look up snapshot");

    info!(log, "able to delete snapshot {} export, proceeding", authz_snapshot.id());

    Ok(())
}

async fn sdd_delete_snapshot_export_record(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let (.., authz_snapshot) =
        LookupPath::new(&opctx, osagactx.datastore())
            .snapshot_id(params.snapshot_id)
            .lookup_for(authz::Action::Delete)
            .await
            .expect("Failed to look up snapshot");

    osagactx
        .datastore()
        .snapshot_export_delete(&opctx, &authz_snapshot, params.snapshot_export_id)
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

/*
// XXX
#[cfg(test)]
pub(crate) mod test {
    use crate::{
        app::saga::create_saga_dag, app::sagas::disk_delete::Params,
        app::sagas::disk_delete::SagaDiskDelete,
    };
    use nexus_db_model::Disk;
    use nexus_db_queries::authn::saga::Serialized;
    use nexus_db_queries::context::OpContext;
    use nexus_test_utils::resource_helpers::create_project;
    use nexus_test_utils::resource_helpers::DiskTest;
    use nexus_test_utils_macros::nexus_test;
    use nexus_types::external_api::params;
    use omicron_common::api::external::Name;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    const PROJECT_NAME: &str = "springfield-squidport";

    pub fn test_opctx(cptestctx: &ControlPlaneTestContext) -> OpContext {
        OpContext::for_tests(
            cptestctx.logctx.log.new(o!()),
            cptestctx.server.server_context().nexus.datastore().clone(),
        )
    }

    async fn create_disk(cptestctx: &ControlPlaneTestContext) -> Disk {
        let nexus = &cptestctx.server.server_context().nexus;
        let opctx = test_opctx(&cptestctx);

        let project_selector = params::ProjectSelector {
            project: Name::try_from(PROJECT_NAME.to_string()).unwrap().into(),
        };
        let project_lookup =
            nexus.project_lookup(&opctx, project_selector).unwrap();

        nexus
            .project_create_disk(
                &opctx,
                &project_lookup,
                &crate::app::sagas::disk_create::test::new_disk_create_params(),
            )
            .await
            .expect("Failed to create disk")
    }

    #[nexus_test(server = crate::Server)]
    async fn test_saga_basic_usage_succeeds(
        cptestctx: &ControlPlaneTestContext,
    ) {
        DiskTest::new(cptestctx).await;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let project_id = create_project(client, PROJECT_NAME).await.identity.id;
        let disk = create_disk(&cptestctx).await;

        // Build the saga DAG with the provided test parameters and run it.
        let opctx = test_opctx(&cptestctx);
        let params = Params {
            serialized_authn: Serialized::for_opctx(&opctx),
            project_id,
            disk_id: disk.id(),
            volume_id: disk.volume_id(),
        };
        nexus.sagas.saga_execute::<SagaDiskDelete>(params).await.unwrap();
    }

    #[nexus_test(server = crate::Server)]
    async fn test_actions_succeed_idempotently(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let test = DiskTest::new(cptestctx).await;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let project_id = create_project(client, PROJECT_NAME).await.identity.id;
        let disk = create_disk(&cptestctx).await;

        // Build the saga DAG with the provided test parameters
        let opctx = test_opctx(&cptestctx);
        let params = Params {
            serialized_authn: Serialized::for_opctx(&opctx),
            project_id,
            disk_id: disk.id(),
            volume_id: disk.volume_id(),
        };
        let dag = create_saga_dag::<SagaDiskDelete>(params).unwrap();
        crate::app::sagas::test_helpers::actions_succeed_idempotently(
            nexus, dag,
        )
        .await;

        crate::app::sagas::disk_create::test::verify_clean_slate(
            &cptestctx, &test,
        )
        .await;
    }
}
*/
