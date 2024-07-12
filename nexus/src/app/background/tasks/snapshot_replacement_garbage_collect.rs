// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background task for cleaning up snapshot replacement step volumes

use crate::app::authn;
use crate::app::background::BackgroundTask;
use crate::app::saga::StartSaga;
use crate::app::sagas;
use crate::app::sagas::snapshot_replacement_garbage_collect::SagaSnapshotReplacementGarbageCollect;
use crate::app::sagas::NexusSaga;
use futures::future::BoxFuture;
use futures::FutureExt;
use nexus_db_model::SnapshotReplacementStep;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db::DataStore;
use nexus_types::internal_api::background::SnapshotReplacementGarbageCollectStatus;
use serde_json::json;
use std::sync::Arc;

pub struct SnapshotReplacementGarbageCollect {
    datastore: Arc<DataStore>,
    sagas: Arc<dyn StartSaga>,
}

impl SnapshotReplacementGarbageCollect {
    pub fn new(datastore: Arc<DataStore>, sagas: Arc<dyn StartSaga>) -> Self {
        SnapshotReplacementGarbageCollect { datastore, sagas }
    }

    async fn send_delete_request(
        &self,
        opctx: &OpContext,
        request: SnapshotReplacementStep,
    ) -> Result<(), omicron_common::api::external::Error> {
        let Some(old_snapshot_volume_id) = request.old_snapshot_volume_id
        else {
            // This state is illegal!
            let s = format!(
                "request {} old snapshot volume id is None!",
                request.id,
            );

            return Err(omicron_common::api::external::Error::internal_error(
                &s,
            ));
        };

        let params = sagas::snapshot_replacement_garbage_collect::Params {
            serialized_authn: authn::saga::Serialized::for_opctx(opctx),
            old_snapshot_volume_id,
            request,
        };

        let saga_dag = SagaSnapshotReplacementGarbageCollect::prepare(&params)?;
        self.sagas.saga_start(saga_dag).await
    }

    async fn clean_up_snapshot_replacement_step_volumes(
        &self,
        opctx: &OpContext,
        status: &mut SnapshotReplacementGarbageCollectStatus,
    ) {
        let log = &opctx.log;

        let requests = match self
            .datastore
            .snapshot_replacement_steps_requiring_garbage_collection(opctx)
            .await
        {
            Ok(requests) => requests,

            Err(e) => {
                let s = format!("querying for steps to collect failed! {e}");
                error!(&log, "{s}");
                status.errors.push(s);
                return;
            }
        };

        for request in requests {
            let request_id = request.id;

            let result = self.send_delete_request(opctx, request).await;

            match result {
                Ok(()) => {
                    let s = format!("delete request ok for {request_id}");

                    info!(&log, "{s}");
                    status.volume_deletes_requested.push(s);
                }

                Err(e) => {
                    let s = format!(
                        "sending snapshot replacement finish \
                        request failed: {e}",
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);
                }
            }
        }
    }
}

impl BackgroundTask for SnapshotReplacementGarbageCollect {
    fn activate<'a>(
        &'a mut self,
        opctx: &'a OpContext,
    ) -> BoxFuture<'a, serde_json::Value> {
        async move {
            let log = &opctx.log;
            info!(&log, "snapshot replacement garbage collect task started",);

            let mut status = SnapshotReplacementGarbageCollectStatus::default();

            self.clean_up_snapshot_replacement_step_volumes(opctx, &mut status)
                .await;

            info!(&log, "snapshot replacement garbage collect task done");

            json!(status)
        }
        .boxed()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::app::background::init::test::NoopStartSaga;
    use nexus_db_model::SnapshotReplacement;
    use nexus_db_model::SnapshotReplacementState;
    use nexus_db_model::SnapshotReplacementStepState;
    use nexus_test_utils_macros::nexus_test;
    use uuid::Uuid;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    #[nexus_test(server = crate::Server)]
    async fn test_snapshot_replacement_garbage_collect_task(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        let starter = Arc::new(NoopStartSaga::new());
        let mut task = SnapshotReplacementGarbageCollect::new(
            datastore.clone(),
            starter.clone(),
        );

        // Noop test
        let result: SnapshotReplacementGarbageCollectStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();
        assert_eq!(result, SnapshotReplacementGarbageCollectStatus::default());
        assert_eq!(starter.count_reset(), 0);

        // Add a snapshot request with two steps that need garbage collection

        let mut request = SnapshotReplacement::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        request.replacement_state = SnapshotReplacementState::Complete;

        let request_id = request.id;

        datastore
            .insert_snapshot_replacement_request_with_volume_id(
                &opctx,
                request,
                Uuid::new_v4(),
            )
            .await
            .unwrap();

        assert!(datastore
            .snapshot_replacement_steps_requiring_garbage_collection(&opctx,)
            .await
            .unwrap()
            .is_empty());

        let mut step = SnapshotReplacementStep::new(request_id, Uuid::new_v4());
        step.replacement_state = SnapshotReplacementStepState::Complete;
        step.old_snapshot_volume_id = Some(Uuid::new_v4());

        let step_1_id = step.id;
        datastore.insert_snapshot_replacement_step(&opctx, step).await.unwrap();

        let mut step = SnapshotReplacementStep::new(request_id, Uuid::new_v4());
        step.replacement_state = SnapshotReplacementStepState::Complete;
        step.old_snapshot_volume_id = Some(Uuid::new_v4());

        let step_2_id = step.id;
        datastore.insert_snapshot_replacement_step(&opctx, step).await.unwrap();

        // Activate the task - it should pick up the two steps

        let result: SnapshotReplacementGarbageCollectStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();

        for error in &result.errors {
            eprintln!("{error}");
        }

        assert_eq!(result.volume_deletes_requested.len(), 2);

        let s = format!("delete request ok for {step_1_id}");
        assert!(result.volume_deletes_requested.contains(&s));

        let s = format!("delete request ok for {step_2_id}");
        assert!(result.volume_deletes_requested.contains(&s));

        assert_eq!(result.errors.len(), 0);

        assert_eq!(starter.count_reset(), 2);
    }
}
