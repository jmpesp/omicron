// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background task for detecting when a snapshot replacement has all its steps
//! done, and finishing it.
//!
//! Once all related snapshot replacement steps are done, the snapshot
//! replacement can be completed.

use crate::app::authn;
use crate::app::background::BackgroundTask;
use crate::app::saga::StartSaga;
use crate::app::sagas;
//use crate::app::sagas::snapshot_replacement_finish::SagaSnapshotReplacementFinish;
use crate::app::sagas::NexusSaga;
use futures::future::BoxFuture;
use futures::FutureExt;
use nexus_db_model::SnapshotReplacement;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db::DataStore;
use nexus_types::internal_api::background::SnapshotReplacementFinishStatus;
use serde_json::json;
use std::sync::Arc;

pub struct SnapshotReplacementFinishDetector {
    datastore: Arc<DataStore>,
    sagas: Arc<dyn StartSaga>,
}

impl SnapshotReplacementFinishDetector {
    pub fn new(datastore: Arc<DataStore>, sagas: Arc<dyn StartSaga>) -> Self {
        SnapshotReplacementFinishDetector { datastore, sagas }
    }

    /*
    async fn send_finish_request(
        &self,
        serialized_authn: authn::saga::Serialized,
        request: SnapshotReplacement,
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

        let params = sagas::snapshot_replacement_finish::Params {
            serialized_authn,
            old_snapshot_volume_id,
            request,
        };

        let saga_dag = SagaSnapshotReplacementFinish::prepare(&params)?;
        self.sagas.saga_start(saga_dag).await
    }
    */

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
                //
                // XXX does this need to be transactional? is that above
                // statement true?

                let region_snapshot = match self
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
                // XXX what if code does a checkout of a volume, that volume is
                // deleted, but code does a modification then volume create?
                // if that's not done in a transaction then there could be
                // incorrect reference counts what if volume create detected
                // when a constituent snapshot was missing? write a unit test
                // for this
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

    /*
    async fn finish_done_snapshot_replacements(
        &self,
        opctx: &OpContext,
        status: &mut SnapshotReplacementFinishStatus,
    ) {
        let log = &opctx.log;

        // Trigger finish saga(s) as appropriate
        let requests =
            match self.datastore.get_done_snapshot_replacements(opctx).await {
                Ok(requests) => requests,

                Err(e) => {
                    let s = format!(
                    "query for done snapshot replacement requests failed: {e}"
                );
                    error!(&log, "{s}");
                    status.errors.push(s);
                    return;
                }
            };

        for request in requests {
            let request_id = request.id;

            let result = self
                .send_finish_request(
                    authn::saga::Serialized::for_opctx(opctx),
                    request,
                )
                .await;

            match result {
                Ok(()) => {
                    let s = format!("finish invoked ok for {request_id}");

                    info!(&log, "{s}");
                    status.finish_invoked_ok.push(s);
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
    }*/
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

            // self.finish_done_snapshot_replacements(opctx, &mut status).await;

            info!(&log, "snapshot replacement finish task done");

            json!(status)
        }
        .boxed()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::app::background::init::test::NoopStartSaga;
    use nexus_db_model::RegionSnapshot;
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

        let starter = Arc::new(NoopStartSaga::new());
        let mut task = SnapshotReplacementFinishDetector::new(
            datastore.clone(),
            starter.clone(),
        );

        // Noop test
        let result: SnapshotReplacementFinishStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();
        assert_eq!(result, SnapshotReplacementFinishStatus::default());
        assert_eq!(starter.count_reset(), 0);

        // Add a snapshot replacement request for a fake region snapshot.

        let dataset_id = Uuid::new_v4();
        let region_id = Uuid::new_v4();
        let snapshot_id = Uuid::new_v4();
        let snapshot_addr = String::from("[fd00:1122:3344::101]:9876");

        let fake_region_snapshot = RegionSnapshot::new(
            dataset_id,
            region_id,
            snapshot_id,
            snapshot_addr.clone(),
        );

        datastore.region_snapshot_create(fake_region_snapshot).await.unwrap();

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

        // Transition that to Allocating then to Running

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
            .set_snapshot_replacement_running(
                &opctx,
                request_id,
                operating_saga_id,
                new_region_id,
                old_snapshot_volume_id,
            )
            .await
            .unwrap();

        // Insert a few steps, not all finished yet

        let operating_saga_id = Uuid::new_v4();

        let mut step_1 =
            SnapshotReplacementStep::new(request_id, Uuid::new_v4());
        step_1.replacement_state = SnapshotReplacementStepState::Running;
        step_1.operating_saga_id = Some(operating_saga_id);
        let step_1_id = step_1.id;

        let mut step_2 =
            SnapshotReplacementStep::new(request_id, Uuid::new_v4());
        step_2.replacement_state = SnapshotReplacementStepState::Running;
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
        assert_eq!(starter.count_reset(), 0);

        // Transition one record to Complete, the task should still do nothing

        datastore
            .set_snapshot_replacement_step_complete(
                &opctx,
                step_1_id,
                operating_saga_id,
                Uuid::new_v4(),
            )
            .await
            .unwrap();

        let result: SnapshotReplacementFinishStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();
        assert_eq!(result, SnapshotReplacementFinishStatus::default());
        assert_eq!(starter.count_reset(), 0);

        // Transition the other record to Complete

        datastore
            .set_snapshot_replacement_step_complete(
                &opctx,
                step_2_id,
                operating_saga_id,
                Uuid::new_v4(),
            )
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
                finish_invoked_ok: vec![format!(
                    "finish invoked ok for {request_id}"
                )],
                errors: vec![],
            },
        );

        assert_eq!(starter.count_reset(), 1);
    }
}
