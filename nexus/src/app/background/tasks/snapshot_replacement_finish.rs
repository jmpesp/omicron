// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background task for detecting when a snapshot replacement has all its steps
//! done, and finishing it.
//!
//! Once all related snapshot replacement steps are done, the snapshot
//! replacement can be completed.

use crate::app::background::BackgroundTask;
use futures::future::BoxFuture;
use futures::FutureExt;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db::DataStore;
use nexus_types::internal_api::background::SnapshotReplacementFinishStatus;
use serde_json::json;
use std::sync::Arc;

pub struct SnapshotReplacementFinishDetector {
    datastore: Arc<DataStore>,
}

impl SnapshotReplacementFinishDetector {
    pub fn new(datastore: Arc<DataStore>) -> Self {
        SnapshotReplacementFinishDetector { datastore }
    }

    async fn transition_requests_to_done(
        &self,
        opctx: &OpContext,
        status: &mut SnapshotReplacementFinishStatus,
    ) {
        let log = &opctx.log;

        // Find all snapshot replacement requests in state "Running"
        let requests =
            match self.datastore.get_running_snapshot_replacements(opctx).await
            {
                Ok(requests) => requests,

                Err(e) => {
                    let s = format!(
                        "query for snapshot replacement requests failed: {e}",
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);

                    return;
                }
            };

        for request in requests {
            // Count associated snapshot replacement steps that are not
            // completed.
            let count = match self
                .datastore
                .in_progress_snapshot_replacement_steps(opctx, request.id)
                .await
            {
                Ok(count) => count,

                Err(e) => {
                    let s = format!(
                        "counting non-complete snapshot replacement steps \
                        failed: {e}",
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);

                    continue;
                }
            };

            if count == 0 {
                // If the region snapshot has been deleted, then the snapshot
                // replacement is done: the reference number went to zero and it
                // was deleted, therefore there aren't any volumes left that
                // reference it!
                match self
                    .datastore
                    .region_snapshot_get(
                        request.old_dataset_id,
                        request.old_region_id,
                        request.old_snapshot_id,
                    )
                    .await
                {
                    Ok(Some(_)) => {
                        continue;
                    }

                    Ok(None) => {
                        // gone!
                    }

                    Err(e) => {
                        let s = format!(
                            "error querying for region snapshot {} {} {}: {e}",
                            request.old_dataset_id,
                            request.old_region_id,
                            request.old_snapshot_id,
                        );
                        error!(&log, "{s}");
                        status.errors.push(s);

                        continue;
                    }
                };

                // Transition snapshot replacement to Complete
                match self
                    .datastore
                    .set_snapshot_replacement_complete(opctx, request.id)
                    .await
                {
                    Ok(()) => {
                        let s = format!("set request {} to done", request.id);
                        info!(&log, "{s}");
                        status.records_set_to_done.push(s);
                    }

                    Err(e) => {
                        let s = format!(
                            "marking snapshot replacement as done failed: {e}"
                        );
                        error!(&log, "{s}");
                        status.errors.push(s);
                    }
                }
            }
        }
    }
}

impl BackgroundTask for SnapshotReplacementFinishDetector {
    fn activate<'a>(
        &'a mut self,
        opctx: &'a OpContext,
    ) -> BoxFuture<'a, serde_json::Value> {
        async move {
            let log = &opctx.log;
            info!(&log, "snapshot replacement finish task started",);

            let mut status = SnapshotReplacementFinishStatus::default();

            self.transition_requests_to_done(opctx, &mut status).await;

            info!(&log, "snapshot replacement finish task done");

            json!(status)
        }
        .boxed()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use nexus_db_model::SnapshotReplacement;
    use nexus_db_model::SnapshotReplacementStep;
    use nexus_db_model::SnapshotReplacementStepState;
    use nexus_test_utils_macros::nexus_test;
    use uuid::Uuid;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    #[nexus_test(server = crate::Server)]
    async fn test_done_snapshot_replacement_causes_finish(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        let mut task =
            SnapshotReplacementFinishDetector::new(datastore.clone());

        // Noop test
        let result: SnapshotReplacementFinishStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();
        assert_eq!(result, SnapshotReplacementFinishStatus::default());

        // Add a snapshot replacement request for a fake region snapshot.

        let dataset_id = Uuid::new_v4();
        let region_id = Uuid::new_v4();
        let snapshot_id = Uuid::new_v4();

        // Do not add the fake region snapshot to the database, as it should
        // have been deleted by the time the request transitions to "Running"

        let request =
            SnapshotReplacement::new(dataset_id, region_id, snapshot_id);

        let request_id = request.id;

        datastore
            .insert_snapshot_replacement_request_with_volume_id(
                &opctx,
                request,
                Uuid::new_v4(),
            )
            .await
            .unwrap();

        // Transition that to Allocating -> ReplacementDone -> DeletingOldVolume
        // -> Running

        let operating_saga_id = Uuid::new_v4();

        datastore
            .set_snapshot_replacement_allocating(
                &opctx,
                request_id,
                operating_saga_id,
            )
            .await
            .unwrap();

        let new_region_id = Uuid::new_v4();
        let old_snapshot_volume_id = Uuid::new_v4();

        datastore
            .set_snapshot_replacement_replacement_done(
                &opctx,
                request_id,
                operating_saga_id,
                new_region_id,
                old_snapshot_volume_id,
            )
            .await
            .unwrap();

        datastore
            .set_snapshot_replacement_deleting_old_volume(
                &opctx,
                request_id,
                operating_saga_id,
            )
            .await
            .unwrap();

        datastore
            .set_snapshot_replacement_running(
                &opctx,
                request_id,
                operating_saga_id,
            )
            .await
            .unwrap();

        // Insert a few steps, not all finished yet

        let operating_saga_id = Uuid::new_v4();

        let mut step_1 =
            SnapshotReplacementStep::new(request_id, Uuid::new_v4());
        step_1.replacement_state = SnapshotReplacementStepState::Complete;
        step_1.operating_saga_id = Some(operating_saga_id);
        let step_1_id = step_1.id;

        let mut step_2 =
            SnapshotReplacementStep::new(request_id, Uuid::new_v4());
        step_2.replacement_state = SnapshotReplacementStepState::Complete;
        step_2.operating_saga_id = Some(operating_saga_id);
        let step_2_id = step_2.id;

        datastore
            .insert_snapshot_replacement_step(&opctx, step_1)
            .await
            .unwrap();
        datastore
            .insert_snapshot_replacement_step(&opctx, step_2)
            .await
            .unwrap();

        // Activate the task, it should do nothing yet

        let result: SnapshotReplacementFinishStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();
        assert_eq!(result, SnapshotReplacementFinishStatus::default());

        // Transition one record to Complete, the task should still do nothing

        datastore
            .set_snapshot_replacement_step_volume_deleted(&opctx, step_1_id)
            .await
            .unwrap();

        let result: SnapshotReplacementFinishStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();
        assert_eq!(result, SnapshotReplacementFinishStatus::default());

        // Transition the other record to Complete

        datastore
            .set_snapshot_replacement_step_volume_deleted(&opctx, step_2_id)
            .await
            .unwrap();

        // Activate the task - it should pick the request up, change the state,
        // and try to run the snapshot replacement finish saga
        let result: SnapshotReplacementFinishStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();

        assert_eq!(
            result,
            SnapshotReplacementFinishStatus {
                records_set_to_done: vec![format!(
                    "set request {request_id} to done"
                )],
                errors: vec![],
            },
        );
    }
}
