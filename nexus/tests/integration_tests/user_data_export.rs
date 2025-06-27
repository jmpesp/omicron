// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests basic user data export support in the API

use crate::integration_tests::images::get_image_create;
use crate::integration_tests::images::get_project_images_url;
use dropshot::test_util::ClientTestContext;
use http::StatusCode;
use http::method::Method;
use nexus_db_lookup::LookupPath;
use nexus_db_model::UserDataExportState;
use nexus_db_queries::authz;
use nexus_db_queries::context::OpContext;
use nexus_test_utils::background::run_user_data_export_coordinator;
use nexus_test_utils::http_testing::AuthnMode;
use nexus_test_utils::http_testing::NexusRequest;
use nexus_test_utils::http_testing::RequestBuilder;
use nexus_test_utils::resource_helpers::create_default_ip_pool;
use nexus_test_utils::resource_helpers::create_project;
use nexus_test_utils::resource_helpers::object_create;
use nexus_test_utils_macros::nexus_test;
use nexus_types::external_api::params;
use nexus_types::external_api::views;
use omicron_common::api::external::ByteCount;
use omicron_common::api::external::Disk;
use omicron_common::api::external::IdentityMetadataCreateParams;
use omicron_common::api::external::Name;
use omicron_test_utils::dev::poll::{CondCheckError, wait_for_condition};
use uuid::Uuid;

type ControlPlaneTestContext =
    nexus_test_utils::ControlPlaneTestContext<omicron_nexus::Server>;
type DiskTest<'a> =
    nexus_test_utils::resource_helpers::DiskTest<'a, omicron_nexus::Server>;

const PROJECT_NAME: &str = "bobs-barrel-of-bits";

// Max chunk size that the Pantry supports
const CHUNK_SIZE: usize = 512 * 1024;

fn get_disks_url() -> String {
    format!("/v1/disks?project={}", PROJECT_NAME)
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
    create_project_and_pool(client).await;
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

    let _: Disk = NexusRequest::new(
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

    // Should be unable to start an export for a non-existent snapshot
    let read_url = format!("/v1/snapshots/{}/read", Uuid::new_v4());
    NexusRequest::new(
        RequestBuilder::new(client, Method::GET, &read_url)
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
    let snapshot_url = format!("/v1/snapshots/{snapshot_id}");
    let read_url = format!("{snapshot_url}/read");

    // Wait for user data export object to be created
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_snapshot(&opctx, snapshot_id)
                    .await
                    .unwrap();

                match object {
                    Some(object) => {
                        if object.state() == UserDataExportState::Live {
                            Ok(())
                        } else {
                            Err(CondCheckError::<()>::NotYet)
                        }
                    }

                    None => Err(CondCheckError::<()>::NotYet),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object created");

    // Grab all blocks (note this buffers 1G in memory)
    let data: bytes::Bytes = NexusRequest::new(
        RequestBuilder::new(client, Method::GET, &read_url)
            .expect_status(Some(StatusCode::OK))
            .expect_response_header(
                http::header::CONTENT_TYPE,
                "application/octet-stream",
            )
            .expect_response_header(
                http::header::CONTENT_DISPOSITION,
                "attachment",
            ),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed snapshot full read")
    .body;

    let size: usize = snapshot.size.to_bytes() as usize;

    assert_eq!(data.len(), size);

    for start in (0..size).step_by(CHUNK_SIZE) {
        // Nexus will proxy requests to the Pantry in CHUNK_SIZE chunks. Check
        // for those there. The simulated pantry will return all 0, plus some
        // breadcrumbs for validation. First the offset, then the chunk size.
        assert_eq!(
            u64::from_le_bytes(data[start..(start + 8)].try_into().unwrap()),
            start as u64,
        );
        assert_eq!(
            usize::from_le_bytes(
                data[(start + 8)..(start + 16)].try_into().unwrap()
            ),
            CHUNK_SIZE,
        );

        assert_eq!(
            data[(start + 16)..(start + CHUNK_SIZE)].len(),
            CHUNK_SIZE - 16
        );

        assert_eq!(
            data[(start + 16)..(start + CHUNK_SIZE)],
            vec![0u8; CHUNK_SIZE - 16]
        );
    }

    // Delete snapshot
    NexusRequest::new(
        RequestBuilder::new(client, Method::DELETE, &snapshot_url)
            .expect_status(Some(StatusCode::NO_CONTENT)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Wait for user data export object to be deleted
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_snapshot(&opctx, snapshot_id)
                    .await
                    .unwrap();

                match object {
                    Some(_) => Err(CondCheckError::<()>::NotYet),
                    None => Ok(()),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object deleted");
}

#[nexus_test]
async fn test_user_data_export_basic_ranged(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.server_context().nexus;
    let datastore = nexus.datastore();
    let opctx =
        OpContext::for_tests(cptestctx.logctx.log.new(o!()), datastore.clone());

    DiskTest::new(&cptestctx).await;
    create_project_and_pool(client).await;
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

    let _: Disk = NexusRequest::new(
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
    let snapshot_url = format!("/v1/snapshots/{snapshot_id}");
    let read_url = format!("{snapshot_url}/read");

    let (.., db_snapshot) = LookupPath::new(&opctx, datastore)
        .snapshot_id(snapshot_id)
        .fetch_for(authz::Action::Read)
        .await
        .unwrap();

    // Wait for user data export object to be created
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_snapshot(&opctx, snapshot_id)
                    .await
                    .unwrap();

                match object {
                    Some(object) => {
                        if object.state() == UserDataExportState::Live {
                            Ok(())
                        } else {
                            Err(CondCheckError::<()>::NotYet)
                        }
                    }

                    None => Err(CondCheckError::<()>::NotYet),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object created");

    // Grab all blocks by chunks
    let snapshot_size: usize = db_snapshot.size.to_bytes() as usize;

    for start in (0..snapshot_size).step_by(CHUNK_SIZE) {
        let end = std::cmp::min(start + CHUNK_SIZE, snapshot_size) - 1;
        let range = format!("bytes={}-{}", start, end);

        let data: bytes::Bytes = NexusRequest::new(
            RequestBuilder::new(client, Method::GET, &read_url)
                .expect_status(Some(StatusCode::PARTIAL_CONTENT))
                .header(http::header::RANGE, &range)
                .expect_range_requestable("application/octet-stream")
                .expect_response_header(
                    http::header::CONTENT_RANGE,
                    &format!("bytes {start}-{end}/{snapshot_size}"),
                )
                .expect_response_header(
                    http::header::CONTENT_DISPOSITION,
                    "attachment",
                ),
        )
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .expect("failed snapshot ranged read")
        .body;

        assert_eq!(data.len(), CHUNK_SIZE);

        // The simulated pantry will return all 0, plus some breadcrumbs for
        // validation. First the offset, then the chunk size.
        assert_eq!(
            u64::from_le_bytes(data[0..8].try_into().unwrap()),
            start as u64
        );
        assert_eq!(
            usize::from_le_bytes(data[8..16].try_into().unwrap()),
            CHUNK_SIZE
        );
        assert_eq!(data[16..], vec![0u8; CHUNK_SIZE - 16]);
    }

    // Delete snapshot
    NexusRequest::new(
        RequestBuilder::new(client, Method::DELETE, &snapshot_url)
            .expect_status(Some(StatusCode::NO_CONTENT)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Wait for user data export object to be deleted
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_snapshot(&opctx, snapshot_id)
                    .await
                    .unwrap();

                match object {
                    Some(_) => Err(CondCheckError::<()>::NotYet),
                    None => Ok(()),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object deleted");
}

/// Test that user data export does not work until saga runs
#[nexus_test]
async fn test_user_data_export_before_creation(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;

    DiskTest::new(&cptestctx).await;
    create_project_and_pool(client).await;
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

    let _: Disk = NexusRequest::new(
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
    let snapshot_url = format!("/v1/snapshots/{snapshot_id}");
    let read_url = format!("{snapshot_url}/read");

    // Try to grab a single block
    NexusRequest::new(
        RequestBuilder::new(client, Method::GET, &read_url)
            .expect_status(Some(StatusCode::BAD_GATEWAY)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed snapshot ranged read");
}

/// Test that user data export does not work after resource deletion
#[nexus_test]
async fn test_user_data_export_after_delete(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.server_context().nexus;
    let datastore = nexus.datastore();
    let opctx =
        OpContext::for_tests(cptestctx.logctx.log.new(o!()), datastore.clone());

    DiskTest::new(&cptestctx).await;
    create_project_and_pool(client).await;
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

    let _: Disk = NexusRequest::new(
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
    let snapshot_url = format!("/v1/snapshots/{snapshot_id}");
    let read_url = format!("{snapshot_url}/read");

    let (.., db_snapshot) = LookupPath::new(&opctx, datastore)
        .snapshot_id(snapshot_id)
        .fetch_for(authz::Action::Read)
        .await
        .unwrap();

    // Wait for user data export object to be created
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_snapshot(&opctx, snapshot_id)
                    .await
                    .unwrap();

                match object {
                    Some(object) => {
                        if object.state() == UserDataExportState::Live {
                            Ok(())
                        } else {
                            Err(CondCheckError::<()>::NotYet)
                        }
                    }

                    None => Err(CondCheckError::<()>::NotYet),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object created");

    // Grab a single block
    let snapshot_size: usize = db_snapshot.size.to_bytes() as usize;

    let start = 512;
    let end = 1023;
    let range = format!("bytes={}-{}", start, end);

    let data: bytes::Bytes = NexusRequest::new(
        RequestBuilder::new(client, Method::GET, &read_url)
            .expect_status(Some(StatusCode::PARTIAL_CONTENT))
            .header(http::header::RANGE, &range)
            .expect_range_requestable("application/octet-stream")
            .expect_response_header(
                http::header::CONTENT_RANGE,
                &format!("bytes {start}-{end}/{snapshot_size}"),
            )
            .expect_response_header(
                http::header::CONTENT_DISPOSITION,
                "attachment",
            ),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed snapshot ranged read")
    .body;

    assert_eq!(data.len(), 512);

    // The simulated pantry will return all 0, plus some breadcrumbs for
    // validation. First the offset, then the chunk size.
    assert_eq!(
        u64::from_le_bytes(data[0..8].try_into().unwrap()),
        start as u64
    );
    assert_eq!(usize::from_le_bytes(data[8..16].try_into().unwrap()), 512);
    assert_eq!(data[16..], vec![0u8; 512 - 16]);

    // Delete snapshot
    NexusRequest::new(
        RequestBuilder::new(client, Method::DELETE, &snapshot_url)
            .expect_status(Some(StatusCode::NO_CONTENT)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Wait for user data export object to be deleted
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_snapshot(&opctx, snapshot_id)
                    .await
                    .unwrap();

                match object {
                    Some(_) => Err(CondCheckError::<()>::NotYet),
                    None => Ok(()),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object deleted");

    // Make sure the read no longer works
    NexusRequest::new(
        RequestBuilder::new(client, Method::GET, &read_url)
            .expect_status(Some(StatusCode::NOT_FOUND)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed snapshot ranged read after delete");
}

#[nexus_test]
async fn test_user_data_export_basic_image_ranged(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.server_context().nexus;
    let datastore = nexus.datastore();
    let opctx =
        OpContext::for_tests(cptestctx.logctx.log.new(o!()), datastore.clone());

    DiskTest::new(&cptestctx).await;
    create_project_and_pool(client).await;

    // Create an image
    let image_create_params = get_image_create(
        params::ImageSource::YouCanBootAnythingAsLongAsItsAlpine,
    );
    let images_url = get_project_images_url(PROJECT_NAME);

    let image =
        NexusRequest::objects_post(client, &images_url, &image_create_params)
            .authn_as(AuthnMode::PrivilegedUser)
            .execute_and_parse_unwrap::<views::Image>()
            .await;

    let image_id = image.identity.id;
    let image_url = format!("/v1/images/{image_id}");
    let read_url = format!("{image_url}/read");

    let (.., db_image) = LookupPath::new(&opctx, datastore)
        .project_image_id(image_id)
        .fetch_for(authz::Action::Read)
        .await
        .unwrap();

    // Wait for user data export object to be created
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_image(&opctx, image_id)
                    .await
                    .unwrap();

                match object {
                    Some(object) => {
                        if object.state() == UserDataExportState::Live {
                            Ok(())
                        } else {
                            Err(CondCheckError::<()>::NotYet)
                        }
                    }

                    None => Err(CondCheckError::<()>::NotYet),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object created");

    // Grab all blocks by chunks
    let image_size: usize = db_image.size.to_bytes() as usize;

    for start in (0..image_size).step_by(CHUNK_SIZE) {
        let end = std::cmp::min(start + CHUNK_SIZE, image_size) - 1;
        let range = format!("bytes={}-{}", start, end);

        let data: bytes::Bytes = NexusRequest::new(
            RequestBuilder::new(client, Method::GET, &read_url)
                .expect_status(Some(StatusCode::PARTIAL_CONTENT))
                .header(http::header::RANGE, &range)
                .expect_range_requestable("application/octet-stream")
                .expect_response_header(
                    http::header::CONTENT_RANGE,
                    &format!("bytes {start}-{end}/{image_size}"),
                )
                .expect_response_header(
                    http::header::CONTENT_DISPOSITION,
                    "attachment",
                ),
        )
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .expect("failed image ranged read")
        .body;

        assert_eq!(data.len(), CHUNK_SIZE);

        // The simulated pantry will return all 0, plus some breadcrumbs for
        // validation. First the offset, then the chunk size.
        assert_eq!(
            u64::from_le_bytes(data[0..8].try_into().unwrap()),
            start as u64
        );
        assert_eq!(
            usize::from_le_bytes(data[8..16].try_into().unwrap()),
            CHUNK_SIZE
        );
        assert_eq!(data[16..], vec![0u8; CHUNK_SIZE - 16]);
    }

    // Delete image
    NexusRequest::new(
        RequestBuilder::new(client, Method::DELETE, &image_url)
            .expect_status(Some(StatusCode::NO_CONTENT)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap();

    // Wait for user data export object to be deleted
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_image(&opctx, image_id)
                    .await
                    .unwrap();

                match object {
                    Some(_) => Err(CondCheckError::<()>::NotYet),
                    None => Ok(()),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object deleted");
}

#[nexus_test]
async fn test_user_data_export_basic_image_ranged_with_promote(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let nexus = &cptestctx.server.server_context().nexus;
    let datastore = nexus.datastore();
    let opctx =
        OpContext::for_tests(cptestctx.logctx.log.new(o!()), datastore.clone());

    DiskTest::new(&cptestctx).await;
    create_project_and_pool(client).await;

    // Create a project image
    let image_create_params = get_image_create(
        params::ImageSource::YouCanBootAnythingAsLongAsItsAlpine,
    );
    let images_url = get_project_images_url(PROJECT_NAME);

    let image =
        NexusRequest::objects_post(client, &images_url, &image_create_params)
            .authn_as(AuthnMode::PrivilegedUser)
            .execute_and_parse_unwrap::<views::Image>()
            .await;

    let image_id = image.identity.id;
    let image_url = format!("/v1/images/{image_id}");
    let read_url = format!("{image_url}/read");

    let (.., db_image) = LookupPath::new(&opctx, datastore)
        .project_image_id(image_id)
        .fetch_for(authz::Action::Read)
        .await
        .unwrap();

    // Wait for user data export object to be created
    run_user_data_export_coordinator(&cptestctx.internal_client).await;

    wait_for_condition(
        || {
            let datastore = datastore.clone();
            let opctx =
                OpContext::for_tests(opctx.log.new(o!()), datastore.clone());

            async move {
                let object = datastore
                    .user_data_export_lookup_for_image(&opctx, image_id)
                    .await
                    .unwrap();

                match object {
                    Some(object) => {
                        if object.state() == UserDataExportState::Live {
                            Ok(())
                        } else {
                            Err(CondCheckError::<()>::NotYet)
                        }
                    }

                    None => Err(CondCheckError::<()>::NotYet),
                }
            }
        },
        &std::time::Duration::from_millis(50),
        &std::time::Duration::from_secs(60),
    )
    .await
    .expect("user data export object created");

    // Grab a blocks by chunks
    let image_size: usize = db_image.size.to_bytes() as usize;

    let start = 512;
    let end = 1023;

    let range = format!("bytes={}-{}", start, end);

    let data: bytes::Bytes = NexusRequest::new(
        RequestBuilder::new(client, Method::GET, &read_url)
            .expect_status(Some(StatusCode::PARTIAL_CONTENT))
            .header(http::header::RANGE, &range)
            .expect_range_requestable("application/octet-stream")
            .expect_response_header(
                http::header::CONTENT_RANGE,
                &format!("bytes {start}-{end}/{image_size}"),
            )
            .expect_response_header(
                http::header::CONTENT_DISPOSITION,
                "attachment",
            ),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed image ranged read")
    .body;

    assert_eq!(data.len(), 512);

    // The simulated pantry will return all 0, plus some breadcrumbs for
    // validation. First the offset, then the chunk size.
    assert_eq!(
        u64::from_le_bytes(data[0..8].try_into().unwrap()),
        start as u64
    );
    assert_eq!(usize::from_le_bytes(data[8..16].try_into().unwrap()), 512);
    assert_eq!(data[16..], vec![0u8; 512 - 16]);

    // Promote image to silo image

    let promote_url = format!("/v1/images/{}/promote", image_id);
    NexusRequest::new(
        RequestBuilder::new(client, http::Method::POST, &promote_url)
            .expect_status(Some(http::StatusCode::ACCEPTED)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute_and_parse_unwrap::<views::Image>()
    .await;

    // Do the read again

    let data: bytes::Bytes = NexusRequest::new(
        RequestBuilder::new(client, Method::GET, &read_url)
            .expect_status(Some(StatusCode::PARTIAL_CONTENT))
            .header(http::header::RANGE, &range)
            .expect_range_requestable("application/octet-stream")
            .expect_response_header(
                http::header::CONTENT_RANGE,
                &format!("bytes {start}-{end}/{image_size}"),
            )
            .expect_response_header(
                http::header::CONTENT_DISPOSITION,
                "attachment",
            ),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("failed image ranged read")
    .body;

    assert_eq!(data.len(), 512);

    // The simulated pantry will return all 0, plus some breadcrumbs for
    // validation. First the offset, then the chunk size.
    assert_eq!(
        u64::from_le_bytes(data[0..8].try_into().unwrap()),
        start as u64
    );
    assert_eq!(usize::from_le_bytes(data[8..16].try_into().unwrap()), 512);
    assert_eq!(data[16..], vec![0u8; 512 - 16]);
}
