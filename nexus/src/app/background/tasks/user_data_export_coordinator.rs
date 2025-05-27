// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! TODO

use crate::app::authn;
use crate::app::background::BackgroundTask;
use crate::app::saga::StartSaga;
use crate::app::sagas;
use crate::app::sagas::NexusSaga;
use crate::app::sagas::user_data_export_create::*;
use crate::app::sagas::user_data_export_delete::*;
use futures::FutureExt;
use futures::future::BoxFuture;
use nexus_db_model::UserDataExportResource;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db::DataStore;
use nexus_db_queries::db::datastore::UserDataExportChangeset;
use nexus_types::identity::Asset;
use nexus_types::internal_api::background::UserDataExportCoordinatorStatus;
use omicron_common::api::external::Error;
use serde_json::json;
use std::sync::Arc;

pub struct UserDataExportCoordinator {
    datastore: Arc<DataStore>,
    sagas: Arc<dyn StartSaga>,
}

impl UserDataExportCoordinator {
    pub fn new(datastore: Arc<DataStore>, sagas: Arc<dyn StartSaga>) -> Self {
        UserDataExportCoordinator { datastore, sagas }
    }

    async fn process_user_data_export_changeset(
        &self,
        opctx: &OpContext,
        changeset: &UserDataExportChangeset,
        status: &mut UserDataExportCoordinatorStatus,
    ) {
        let log = &opctx.log;

        for item in &changeset.create_required {
            let params = sagas::user_data_export_create::Params {
                serialized_authn: authn::saga::Serialized::for_opctx(opctx),
                resource: item.clone(),
            };

            let saga_dag = match SagaUserDataExportCreate::prepare(&params) {
                Ok(dag) => dag,

                Err(e) => {
                    let s = format!(
                        "error preparing user data export create dag for \
                        {item:?}: {e}"
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);

                    continue;
                }
            };

            match self.sagas.saga_start(saga_dag).await {
                Ok(id) => {
                    let s = format!(
                        "requested user data export create saga for {item:?}"
                    );
                    info!(&log, "{s}"; "saga_id" => %id);
                    status.create_invoked_ok.push(s);
                }

                Err(e) => {
                    let s = format!(
                        "error requesting user data export create for \
                        {item:?}: {e}"
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);
                }
            }
        }

        for item in &changeset.delete_required {
            let params = sagas::user_data_export_delete::Params {
                serialized_authn: authn::saga::Serialized::for_opctx(opctx),
                user_data_export_id: item.id(),
                volume_id: item.volume_id(),
            };

            let saga_dag = match SagaUserDataExportDelete::prepare(&params) {
                Ok(dag) => dag,

                Err(e) => {
                    let s = format!(
                        "error preparing user data export delete dag for \
                        {item:?}: {e}"
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);

                    continue;
                }
            };

            match self.sagas.saga_start(saga_dag).await {
                Ok(id) => {
                    let s = format!(
                        "requested user data export delete saga for {item:?}"
                    );
                    info!(&log, "{s}"; "saga_id" => %id);
                    status.delete_invoked_ok.push(s);
                }

                Err(e) => {
                    let s = format!(
                        "error requesting user data export delete for \
                        {item:?}: {e}"
                    );
                    error!(&log, "{s}");
                    status.errors.push(s);
                }
            }
        }
    }
}

impl BackgroundTask for UserDataExportCoordinator {
    fn activate<'a>(
        &'a mut self,
        opctx: &'a OpContext,
    ) -> BoxFuture<'a, serde_json::Value> {
        async move {
            let log = &opctx.log;
            let mut status = UserDataExportCoordinatorStatus::default();

            let changeset =
                match self.datastore.user_data_export_changeset(&opctx).await {
                    Ok(changeset) => changeset,

                    Err(e) => {
                        let s = format!(
                            "error getting user data export changeset: {e}"
                        );
                        error!(&log, "{s}");
                        status.errors.push(s);

                        return json!(status);
                    }
                };

            error!(&log, "changeset is {changeset:?}");

            self.process_user_data_export_changeset(
                &opctx,
                &changeset,
                &mut status,
            )
            .await;

            json!(status)
        }
        .boxed()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::app::MIN_DISK_SIZE_BYTES;
    use crate::app::authz;
    use crate::app::background::init::test::NoopStartSaga;
    use chrono::Utc;
    use nexus_db_lookup::LookupPath;
    use nexus_db_model::BlockSize;
    use nexus_db_model::Generation;
    use nexus_db_model::ProjectImage;
    use nexus_db_model::ProjectImageIdentity;
    use nexus_db_model::Snapshot;
    use nexus_db_model::SnapshotIdentity;
    use nexus_db_model::SnapshotState;
    use nexus_test_utils::resource_helpers::create_default_ip_pool;
    use nexus_test_utils::resource_helpers::create_project;
    use nexus_test_utils::resource_helpers::object_create;
    use nexus_test_utils_macros::nexus_test;
    use nexus_types::identity::Resource;
    use omicron_common::api::external;
    use omicron_uuid_kinds::DatasetUuid;
    use omicron_uuid_kinds::GenericUuid;
    use omicron_uuid_kinds::UserDataExportUuid;
    use omicron_uuid_kinds::VolumeUuid;
    use std::net::Ipv6Addr;
    use std::net::SocketAddrV6;
    use uuid::Uuid;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    const PROJECT_NAME: &str = "bobs-barrel-of-bits";

    async fn setup_test_project(
        cptestctx: &ControlPlaneTestContext,
        opctx: &OpContext,
    ) -> authz::Project {
        create_default_ip_pool(&cptestctx.external_client).await;

        let project =
            create_project(&cptestctx.external_client, PROJECT_NAME).await;

        let datastore = cptestctx.server.server_context().nexus.datastore();

        let (_, authz_project) = LookupPath::new(opctx, datastore)
            .project_id(project.identity.id)
            .lookup_for(authz::Action::CreateChild)
            .await
            .expect("project must exist");

        authz_project
    }

    #[nexus_test(server = crate::Server)]
    async fn test_user_data_export_coordinator_task_noop(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        let starter = Arc::new(NoopStartSaga::new());
        let mut task =
            UserDataExportCoordinator::new(datastore.clone(), starter.clone());

        let result: UserDataExportCoordinatorStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();

        assert_eq!(result, UserDataExportCoordinatorStatus::default());
        assert_eq!(starter.count_reset(), 0);
    }

    #[nexus_test(server = crate::Server)]
    async fn test_user_data_export_coordinator_task_create(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        let starter = Arc::new(NoopStartSaga::new());
        let mut task =
            UserDataExportCoordinator::new(datastore.clone(), starter.clone());

        let authz_project = setup_test_project(cptestctx, &opctx).await;

        // Add a snapshot and image

        let snapshot = datastore
            .project_ensure_snapshot(
                &opctx,
                &authz_project,
                Snapshot {
                    identity: SnapshotIdentity {
                        id: Uuid::new_v4(),
                        name: external::Name::try_from("snapshot".to_string())
                            .unwrap()
                            .into(),
                        description: "snapshot".into(),

                        time_created: Utc::now(),
                        time_modified: Utc::now(),
                        time_deleted: None,
                    },

                    project_id: authz_project.id(),
                    disk_id: Uuid::new_v4(),
                    volume_id: VolumeUuid::new_v4().into(),
                    destination_volume_id: VolumeUuid::new_v4().into(),

                    gen: Generation::new(),
                    state: SnapshotState::Creating,
                    block_size: BlockSize::AdvancedFormat,

                    size: external::ByteCount::try_from(
                        2 * MIN_DISK_SIZE_BYTES,
                    )
                    .unwrap()
                    .into(),
                },
            )
            .await
            .unwrap();

        let image = datastore
            .project_image_create(
                &opctx,
                &authz_project,
                ProjectImage {
                    identity: ProjectImageIdentity {
                        id: Uuid::new_v4(),
                        name: external::Name::try_from("image".to_string())
                            .unwrap()
                            .into(),
                        description: "description".into(),

                        time_created: Utc::now(),
                        time_modified: Utc::now(),
                        time_deleted: None,
                    },

                    silo_id: Uuid::new_v4(),
                    project_id: authz_project.id(),
                    volume_id: VolumeUuid::new_v4().into(),

                    url: None,
                    os: String::from("debian"),
                    version: String::from("12"),
                    digest: None,
                    block_size: BlockSize::Iso,

                    size: external::ByteCount::try_from(
                        1 * MIN_DISK_SIZE_BYTES,
                    )
                    .unwrap()
                    .into(),
                },
            )
            .await
            .unwrap();

        // Activate the task - it should try to create user data export objects
        // for the snapshot and image

        let result: UserDataExportCoordinatorStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();

        eprintln!("{result:?}");

        assert_eq!(result.create_invoked_ok.len(), 2);

        let s = format!(
            "{:?}",
            UserDataExportResource::Snapshot { id: snapshot.id() },
        );
        assert!(result.create_invoked_ok.iter().any(|i| i.contains(&s)));

        let s =
            format!("{:?}", UserDataExportResource::Image { id: image.id() },);
        assert!(result.create_invoked_ok.iter().any(|i| i.contains(&s)));

        assert_eq!(result.errors.len(), 0);

        assert_eq!(starter.count_reset(), 2);
    }

    #[nexus_test(server = crate::Server)]
    async fn test_user_data_export_coordinator_task_delete(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let nexus = &cptestctx.server.server_context().nexus;
        let datastore = nexus.datastore();
        let opctx = OpContext::for_tests(
            cptestctx.logctx.log.clone(),
            datastore.clone(),
        );

        let starter = Arc::new(NoopStartSaga::new());
        let mut task =
            UserDataExportCoordinator::new(datastore.clone(), starter.clone());

        let authz_project = setup_test_project(cptestctx, &opctx).await;

        // Add a snapshot and image

        let snapshot = datastore
            .project_ensure_snapshot(
                &opctx,
                &authz_project,
                Snapshot {
                    identity: SnapshotIdentity {
                        id: Uuid::new_v4(),
                        name: external::Name::try_from("snapshot".to_string())
                            .unwrap()
                            .into(),
                        description: "snapshot".into(),

                        time_created: Utc::now(),
                        time_modified: Utc::now(),
                        time_deleted: None,
                    },

                    project_id: authz_project.id(),
                    disk_id: Uuid::new_v4(),
                    volume_id: VolumeUuid::new_v4().into(),
                    destination_volume_id: VolumeUuid::new_v4().into(),

                    gen: Generation::new(),
                    state: SnapshotState::Creating,
                    block_size: BlockSize::AdvancedFormat,

                    size: external::ByteCount::try_from(
                        2 * MIN_DISK_SIZE_BYTES,
                    )
                    .unwrap()
                    .into(),
                },
            )
            .await
            .unwrap();

        let image = datastore
            .project_image_create(
                &opctx,
                &authz_project,
                ProjectImage {
                    identity: ProjectImageIdentity {
                        id: Uuid::new_v4(),
                        name: external::Name::try_from("image".to_string())
                            .unwrap()
                            .into(),
                        description: "description".into(),

                        time_created: Utc::now(),
                        time_modified: Utc::now(),
                        time_deleted: None,
                    },

                    silo_id: Uuid::new_v4(),
                    project_id: authz_project.id(),
                    volume_id: VolumeUuid::new_v4().into(),

                    url: None,
                    os: String::from("debian"),
                    version: String::from("12"),
                    digest: None,
                    block_size: BlockSize::Iso,

                    size: external::ByteCount::try_from(
                        1 * MIN_DISK_SIZE_BYTES,
                    )
                    .unwrap()
                    .into(),
                },
            )
            .await
            .unwrap();

        // Create user data export rows for the snapshot and image

        let (.., authz_snapshot, db_snapshot) =
            LookupPath::new(&opctx, datastore)
                .snapshot_id(snapshot.id())
                .fetch_for(authz::Action::Read)
                .await
                .unwrap();

        datastore
            .user_data_export_create_for_snapshot(
                &opctx,
                UserDataExportUuid::new_v4(),
                &authz_snapshot,
                SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0),
                VolumeUuid::new_v4(),
            )
            .await
            .unwrap();

        let (.., authz_image, db_image) = LookupPath::new(&opctx, datastore)
            .image_id(image.id())
            .fetch_for(authz::Action::Read)
            .await
            .unwrap();

        datastore
            .user_data_export_create_for_image(
                &opctx,
                UserDataExportUuid::new_v4(),
                &authz_image,
                SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0),
                VolumeUuid::new_v4(),
            )
            .await
            .unwrap();

        // Activate the task - it should do nothing, as there are user data
        // export objects already.

        let result: UserDataExportCoordinatorStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();

        assert_eq!(result, UserDataExportCoordinatorStatus::default());
        assert_eq!(starter.count_reset(), 0);

        // Delete the snapshot

        datastore
            .project_delete_snapshot(
                &opctx,
                &authz_snapshot,
                &db_snapshot,
                vec![SnapshotState::Creating, SnapshotState::Ready],
            )
            .await
            .unwrap();

        // Activate the task - it should only try to delete the user data export
        // object associated with the snapshot

        let result: UserDataExportCoordinatorStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();

        eprintln!("{result:?}");

        assert!(result.errors.is_empty());
        assert_eq!(result.create_invoked_ok.len(), 0);
        assert_eq!(result.delete_invoked_ok.len(), 1);

        let s = format!("resource_id: {}", snapshot.id());
        assert!(result.delete_invoked_ok.iter().any(|i| i.contains(&s)));

        let s = format!("resource_id: {}", image.id());
        assert!(result.delete_invoked_ok.iter().all(|i| !i.contains(&s)));

        assert_eq!(result.errors.len(), 0);
        assert_eq!(starter.count_reset(), 1);

        // Delete the image, now it should try to delete both.

        let (.., authz_image, db_image) = LookupPath::new(&opctx, datastore)
            .project_image_id(image.id())
            .fetch_for(authz::Action::Read)
            .await
            .unwrap();

        datastore
            .project_image_delete(&opctx, &authz_image, db_image)
            .await
            .unwrap();

        let result: UserDataExportCoordinatorStatus =
            serde_json::from_value(task.activate(&opctx).await).unwrap();

        assert!(result.errors.is_empty());
        assert_eq!(result.create_invoked_ok.len(), 0);
        assert_eq!(result.delete_invoked_ok.len(), 2);

        let s = format!("resource_id: {}", snapshot.id());
        assert!(result.delete_invoked_ok.iter().any(|i| i.contains(&s)));

        let s = format!("resource_id: {}", image.id());
        assert!(result.delete_invoked_ok.iter().any(|i| i.contains(&s)));

        assert_eq!(result.errors.len(), 0);
        assert_eq!(starter.count_reset(), 2);
    }
}
