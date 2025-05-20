// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests basic snapshot export support in the API

use crate::integration_tests::instances::instance_simulate;
use chrono::Utc;
use dropshot::test_util::ClientTestContext;
use http::method::Method;
use http::StatusCode;
use nexus_config::RegionAllocationStrategy;
use nexus_db_model::to_db_typed_uuid;
use nexus_db_queries::authz;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db;
use nexus_db_queries::db::datastore::RegionAllocationFor;
use nexus_db_queries::db::datastore::RegionAllocationParameters;
use nexus_db_queries::db::datastore::REGION_REDUNDANCY_THRESHOLD;
use nexus_db_queries::db::identity::Resource;
use nexus_db_lookup::LookupPath;
use nexus_test_utils::http_testing::AuthnMode;
use nexus_test_utils::http_testing::NexusRequest;
use nexus_test_utils::http_testing::RequestBuilder;
use nexus_test_utils::resource_helpers::create_default_ip_pool;
use nexus_test_utils::resource_helpers::create_disk;
use nexus_test_utils::resource_helpers::create_project;
use nexus_test_utils::resource_helpers::object_create;
use nexus_test_utils::SLED_AGENT_UUID;
use nexus_test_utils_macros::nexus_test;
use nexus_types::external_api::params;
use nexus_types::external_api::views;
use omicron_common::api::external;
use omicron_common::api::external::ByteCount;
use omicron_common::api::external::Disk;
use omicron_common::api::external::DiskState;
use omicron_common::api::external::IdentityMetadataCreateParams;
use omicron_common::api::external::Instance;
use omicron_common::api::external::InstanceCpuCount;
use omicron_common::api::external::Name;
use omicron_nexus::app::MIN_DISK_SIZE_BYTES;
use omicron_uuid_kinds::DatasetUuid;
use omicron_uuid_kinds::GenericUuid;
use omicron_uuid_kinds::InstanceUuid;
use omicron_uuid_kinds::VolumeUuid;
use uuid::Uuid;

type ControlPlaneTestContext =
    nexus_test_utils::ControlPlaneTestContext<omicron_nexus::Server>;
type DiskTest<'a> =
    nexus_test_utils::resource_helpers::DiskTest<'a, omicron_nexus::Server>;
type DiskTestBuilder<'a> = nexus_test_utils::resource_helpers::DiskTestBuilder<
    'a,
    omicron_nexus::Server,
>;

const PROJECT_NAME: &str = "springfield-squidport-disks";

// Use 8 MiB chunk size so tests won't take a long time.
const CHUNK_SIZE: u32 = 8192 * 1024;

fn get_disks_url() -> String {
    format!("/v1/disks?project={}", PROJECT_NAME)
}

fn get_disk_url(name: &str) -> String {
    format!("/v1/disks/{}?project={}", name, PROJECT_NAME)
}

async fn create_project_and_pool(client: &ClientTestContext) -> Uuid {
    create_default_ip_pool(client).await;
    let project = create_project(client, PROJECT_NAME).await;
    project.identity.id
}

#[nexus_test]
async fn test_user_data_export_basic(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.server_context().nexus;
    let datastore = nexus.datastore();
    let opctx =
        OpContext::for_tests(cptestctx.logctx.log.new(o!()), datastore.clone());

    DiskTest::new(&cptestctx).await;
    let project_id = create_project_and_pool(client).await;
    let disks_url = get_disks_url();

    // Create a blank disk
    let disk_size = ByteCount::from_gibibytes_u32(1);
    let base_disk_name: Name = "base-disk".parse().unwrap();
    let base_disk = params::DiskCreate {
        identity: IdentityMetadataCreateParams {
            name: base_disk_name.clone(),
            description: String::from("sells rainsticks"),
        },
        disk_source: params::DiskSource::Blank {
            block_size: params::BlockSize::try_from(512).unwrap(),
        },
        size: disk_size,
    };

    let base_disk: Disk = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &disks_url)
            .body(Some(&base_disk))
            .expect_status(Some(StatusCode::CREATED)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap()
    .parsed_body()
    .unwrap();

    // Should be unable to start a snapshot export for a non-existent snapshot
    let start_url = format!("/v1/snapshots/{}/bulk-read-start", Uuid::new_v4());
    NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &start_url)
            .expect_status(Some(StatusCode::NOT_FOUND)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Issue snapshot request
    let snapshots_url = format!("/v1/snapshots?project={}", PROJECT_NAME);

    let snapshot: views::Snapshot = object_create(
        client,
        &snapshots_url,
        &params::SnapshotCreate {
            identity: IdentityMetadataCreateParams {
                name: "not-attached".parse().unwrap(),
                description: "not attached to instance".into(),
            },
            disk: base_disk_name.clone().into(),
        },
    )
    .await;

    let snapshot_id = snapshot.identity.id;
    let start_url = format!("/v1/snapshots/{snapshot_id}/bulk-read-start");
    let grab_url = format!("/v1/snapshots/{snapshot_id}/bulk-read");
    let stop_url = format!("/v1/snapshots/{snapshot_id}/bulk-read-stop");
    let snapshot_url = format!("/v1/snapshots/{snapshot_id}");

    // Before starting the export, attempting a bulk read shouldn't work
    NexusRequest::new(
        RequestBuilder::new(client, Method::GET, &grab_url)
            .body(Some(&params::ExportBlocksBulkReadRequest {
                offset: 0,
                size: CHUNK_SIZE,
            }))
            .expect_status(Some(StatusCode::GONE)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Start the export process
    NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &start_url)
            .expect_status(Some(StatusCode::NO_CONTENT)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Trying to start again will not do anything and still return 204
    NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &start_url)
            .expect_status(Some(StatusCode::NO_CONTENT)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Grab blocks
    for offset in (0..snapshot.size.into()).step_by(CHUNK_SIZE as usize) {
        let response: params::ExportBlocksBulkReadResponse = NexusRequest::new(
            RequestBuilder::new(client, Method::GET, &grab_url)
                .body(Some(&params::ExportBlocksBulkReadRequest {
                    offset: offset as u64,
                    size: CHUNK_SIZE,
                }))
                .expect_status(Some(StatusCode::OK)),
        )
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .unwrap()
        .parsed_body()
        .unwrap();

        let data = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &response.base64_encoded_data,
        ).unwrap();

        // The simulated pantry will return all 0
        assert_eq!(data, vec![0u8; CHUNK_SIZE as usize]);
    }

    // XXX can still make disk out of snapshot, even if it's exporting

    // Stop the export process
    NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &stop_url)
            .expect_status(Some(StatusCode::NO_CONTENT)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Trying to stop again will not do anything and return 410
    NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &stop_url)
            .expect_status(Some(StatusCode::GONE)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Delete snapshot
    NexusRequest::new(
        RequestBuilder::new(client, Method::DELETE, &snapshot_url)
            .expect_status(Some(StatusCode::NO_CONTENT)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Should be unable to start an export after the snapshot is deleted
    NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &start_url)
            .expect_status(Some(StatusCode::NOT_FOUND)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();
}

// XXX TEST: expunge pantry that it's on?
// XXX TEST: permissions

