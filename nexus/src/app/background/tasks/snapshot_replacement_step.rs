// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background task for detecting volumes affected by a snapshot replacement,
//! creating records for those, and triggering the "step" saga for them.
//!
//! After the snapshot replacement start saga finishes, the snapshot's volume is
//! no longer in a degraded state: the requested read-only region was cloned to
//! a new region, and the reference was replaced in the construction request.
//! Any disk that is now created using the snapshot as a source will work
//! without issues.
//!
//! The problem now is volumes that still reference the replaced read-only
//! region, and any Upstairs constructed from a VCR that references that region.
//! This task's responsibility is to find all volumes that reference the
//! replaced read-only region, create a record for them, and trigger the
//! snapshot replacement step saga. This is a much less involved process than
//! region replacement: no continuous monitoring and driving is required. See
//! the "snapshot replacement step" saga's docstring for more information.

use crate::app::authn;
use crate::app::background::BackgroundTask;
use crate::app::saga::StartSaga;
use crate::app::sagas;
use crate::app::sagas::snapshot_replacement_step::SagaSnapshotReplacementStep;
use crate::app::sagas::NexusSaga;
use futures::future::BoxFuture;
use futures::FutureExt;
use nexus_db_model::SnapshotReplacementStep;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db::DataStore;
use nexus_types::identity::Asset;
use nexus_types::internal_api::background::SnapshotReplacementStepStatus;
use serde_json::json;
use std::sync::Arc;

pub struct SnapshotReplacementFindAffected {
    datastore: Arc<DataStore>,
    sagas: Arc<dyn StartSaga>,
}

impl SnapshotReplacementFindAffected {
    pub fn new(datastore: Arc<DataStore>, sagas: Arc<dyn StartSaga>) -> Self {
        SnapshotReplacementFindAffected { datastore, sagas }
    }

    async fn send_start_request(
        &self,
        opctx: &OpContext,
        request: SnapshotReplacementStep,
    ) -> Result<(), omicron_common::api::external::Error> {
        let params = sagas::snapshot_replacement_step::Params {
            serialized_authn: authn::saga::Serialized::for_opctx(opctx),
            request,
        };

        let saga_dag = SagaSnapshotReplacementStep::prepare(&params)?;
        self.sagas.saga_start(saga_dag).await
    }

    async fn create_step_records_for_affected_volumes(
        &self,
        opctx: &OpContext,
        status: &mut SnapshotReplacementStepStatus,
    ) {
        let log = &opctx.log;

        // Find all snapshot replacement requests in state "Running"
        let requests =
            match self.datastore.get_running_snapshot_replacements(opctx).await
            {
                Ok(requests) => requests,

                Err(e) => {
                    let s = format!(
                        "get_running_snapshot_replacements failed: {e}",
                    );

                    error!(&log, "{s}");
                    status.errors.push(s);
                    return;
                }
            };

        for request in requests {
            // Find all volumes that reference the replaced region
            let region_snapshot = match self
                .datastore
                .region_snapshot_get(
                    request.old_dataset_id,
                    request.old_region_id,
                    request.old_snapshot_id,
                )
                .await
            {
                Ok(Some(region_snapshot)) => region_snapshot,

                Ok(None) => {
                    let s = format!(
                        "region snapshot {} {} {} not found!",
                        request.old_dataset_id,
                        request.old_region_id,
                        request.old_snapshot_id,
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);

                    continue;
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

            let snapshot_addr = match region_snapshot.snapshot_addr.parse() {
                Ok(addr) => addr,

                Err(e) => {
                    let s = format!(
                        "region snapshot addr {} could not be parsed: {e}",
                        region_snapshot.snapshot_addr,
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);

                    continue;
                }
            };

            let volumes = match self
                .datastore
                .find_volumes_referencing_socket_addr(&opctx, snapshot_addr)
                .await
            {
                Ok(volumes) => volumes,

                Err(e) => {
                    let s = format!("error finding referenced volumes: {e}");
                    error!(
                        log,
                        "{s}";
                        "request id" => ?request.id,
                    );
                    status.errors.push(s);

                    continue;
                }
            };

            for volume in volumes {
                // Any volume referencing the old socket addr needs to be
                // replaced. Create a record for this.
                match self
                    .datastore
                    .create_snapshot_replacement_step(
                        opctx,
                        request.id,
                        volume.id(),
                    )
                    .await
                {
                    Ok(volume_request_id) => {
                        let s = format!("created {volume_request_id}");
                        info!(
                            log,
                            "{s}";
                            "request id" => ?request.id,
                            "volume id" => ?volume.id(),
                        );
                        status.step_records_created_ok.push(s);
                    }

                    Err(e) => {
                        let s = format!("error creating volume request: {e}");
                        error!(
                            log,
                            "{s}";
                            "request id" => ?request.id,
                            "volume id" => ?volume.id(),
                        );
                        status.errors.push(s);
                    }
                }
            }
        }
    }

    async fn invoke_step_saga_for_affected_volumes(
        &self,
        opctx: &OpContext,
        status: &mut SnapshotReplacementStepStatus,
    ) {
        let log = &opctx.log;

        // Once all snapshot replace volume requests have been sent, trigger
        // sagas as appropriate

        let step_requests = match self
            .datastore
            .get_requested_snapshot_replacement_steps(opctx)
            .await
        {
            Ok(step_requests) => step_requests,

            Err(e) => {
                let s = format!(
                    "query for requested snapshot replacement step requests \
                    failed: {e}"
                );
                error!(&log, "{s}");
                status.errors.push(s);

                return;
            }
        };

        for request in step_requests {
            let request_id = request.id;

            match self.send_start_request(opctx, request).await {
                Ok(()) => {
                    let s = format!("step invoked ok for {request_id}");

                    info!(&log, "{s}");
                    status.step_invoked_ok.push(s);
                }

                Err(e) => {
                    let s = format!(
                        "sending snapshot replacement step request failed: {e}"
                    );

                    error!(&log, "{s}");
                    status.errors.push(s);
                }
            };
        }
    }
}

impl BackgroundTask for SnapshotReplacementFindAffected {
    fn activate<'a>(
        &'a mut self,
        opctx: &'a OpContext,
    ) -> BoxFuture<'a, serde_json::Value> {
        async move {
            let log = &opctx.log;
            info!(
                &log,
                "snapshot replacement find affected volumes task started",
            );

            let mut status = SnapshotReplacementStepStatus::default();

            self.create_step_records_for_affected_volumes(opctx, &mut status)
                .await;

            self.invoke_step_saga_for_affected_volumes(opctx, &mut status)
                .await;

            info!(&log, "snapshot replacement find affected volumes task done");

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
    use nexus_db_model::Volume;
    use nexus_test_utils_macros::nexus_test;
    use sled_agent_client::types::CrucibleOpts;
    use sled_agent_client::types::VolumeConstructionRequest;
    use uuid::Uuid;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    async fn add_fake_volume_for_snapshot_addr(
        datastore: &DataStore,
        snapshot_addr: String,
    ) -> Uuid {
        let new_volume_id = Uuid::new_v4();

        let volume_construction_request = VolumeConstructionRequest::Volume {
            id: new_volume_id,
            block_size: 0,
            sub_volumes: vec![],
            read_only_parent: Some(Box::new(
                VolumeConstructionRequest::Region {
                    block_size: 0,
                    blocks_per_extent: 0,
                    extent_count: 0,
                    gen: 0,
                    opts: CrucibleOpts {
                        id: Uuid::new_v4(),
                        target: vec![snapshot_addr],
                        lossy: false,
                        flush_timeout: None,
                        key: None,
                        cert_pem: None,
                        key_pem: None,
                        root_cert_pem: None,
                        control: None,
                        read_only: true,
                    },
                },
            )),
        };

        let volume_data =
            serde_json::to_string(&volume_construction_request).unwrap();

        let volume = Volume::new(new_volume_id, volume_data);

        datastore.volume_create(volume).await.unwrap();

        new_volume_id
    }

    #[nexus_test(server = crate::Server)]
    async fn test_snapshot_replacement_step_task(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        let starter = Arc::new(NoopStartSaga::new());
        let mut task = SnapshotReplacementFindAffected::new(
            datastore.clone(),
            starter.clone(),
        );

        // Noop test
        let result: SnapshotReplacementStepStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();
        assert_eq!(result, SnapshotReplacementStepStatus::default());
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

        // Add some fake volumes that reference the snapshot being replaced

        let new_volume_1_id = add_fake_volume_for_snapshot_addr(
            &datastore,
            snapshot_addr.clone(),
        )
        .await;
        let new_volume_2_id = add_fake_volume_for_snapshot_addr(
            &datastore,
            snapshot_addr.clone(),
        )
        .await;

        // Add some fake volumes that do not

        let other_volume_1_id = add_fake_volume_for_snapshot_addr(
            &datastore,
            String::from("[fd00:1122:3344::101]:1000"),
        )
        .await;

        let other_volume_2_id = add_fake_volume_for_snapshot_addr(
            &datastore,
            String::from("[fd12:5544:3344::912]:3901"),
        )
        .await;

        // Activate the task - it should pick the running request up and try to
        // run the snapshot replacement step saga for the volumes

        let result: SnapshotReplacementStepStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();

        let requested_snapshot_replacement_steps = datastore
            .get_requested_snapshot_replacement_steps(&opctx)
            .await
            .unwrap();

        assert_eq!(requested_snapshot_replacement_steps.len(), 2);

        for step in &requested_snapshot_replacement_steps {
            let s: String = format!("created {}", step.id);
            assert!(result.step_records_created_ok.contains(&s));

            let s: String = format!("step invoked ok for {}", step.id);
            assert!(result.step_invoked_ok.contains(&s));

            if step.volume_id == new_volume_1_id
                || step.volume_id == new_volume_2_id
            {
                // ok!
            } else if step.volume_id == other_volume_1_id
                || step.volume_id == other_volume_2_id
            {
                // error!
                assert!(false);
            } else {
                // error!
                assert!(false);
            }
        }

        assert_eq!(result.errors.len(), 0);

        assert_eq!(starter.count_reset(), 2);
    }
}
