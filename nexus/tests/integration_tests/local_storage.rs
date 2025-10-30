// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests local storage support

use crate::integration_tests::instances::get_instance_url;
use crate::integration_tests::instances::instance_get;
use crate::integration_tests::instances::instance_simulate;
use dropshot::HttpErrorResponseBody;
use dropshot::test_util::ClientTestContext;
use http::StatusCode;
use http::method::Method;
use nexus_test_utils::background::run_blueprint_executor;
use nexus_test_utils::background::run_blueprint_loader;
use nexus_test_utils::background::run_blueprint_planner;
use nexus_test_utils::background::run_blueprint_rendezvous;
use nexus_test_utils::http_testing::AuthnMode;
use nexus_test_utils::http_testing::NexusRequest;
use nexus_test_utils::http_testing::RequestBuilder;
use nexus_test_utils::resource_helpers::create_default_ip_pool;
use nexus_test_utils::resource_helpers::create_project;
use nexus_test_utils_macros::nexus_test;
use nexus_types::external_api::{params, views};
use nexus_types::internal_api::background::BlueprintPlannerStatus;
use nexus_types::internal_api::params::InstanceMigrateRequest;
use nexus_types::deployment::BlueprintTargetSet;
use omicron_common::api::external;
use omicron_common::api::external::ByteCount;
use omicron_common::api::external::IdentityMetadataCreateParams;
use omicron_common::api::external::InstanceState;
use omicron_nexus::TestInterfaces as _;
use omicron_nexus::app::MAX_DISK_SIZE_BYTES;
use omicron_nexus::app::MIN_DISK_SIZE_BYTES;
use omicron_uuid_kinds::GenericUuid;
use omicron_uuid_kinds::InstanceUuid;
use std::convert::TryFrom;
use nexus_auth::context::OpContext;

type ControlPlaneTestContext =
    nexus_test_utils::ControlPlaneTestContext<omicron_nexus::Server>;

type DiskTest<'a> =
    nexus_test_utils::resource_helpers::DiskTest<'a, omicron_nexus::Server>;

static PROJECT_NAME: &str = "springfield-squidport";

fn get_project_selector() -> String {
    format!("project={}", PROJECT_NAME)
}

fn get_disks_url() -> String {
    format!("/v1/disks?{}", get_project_selector())
}

pub async fn create_project_and_pool(
    client: &ClientTestContext,
) -> views::Project {
    create_default_ip_pool(client).await;
    create_project(client, PROJECT_NAME).await
}

// Test the various ways Nexus can reject a local storage disk based on sizes
#[nexus_test]
async fn test_reject_creating_local_storage_disk(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;

    // Create some disks
    DiskTest::new(&cptestctx).await;

    create_project_and_pool(&client).await;

    let disks_url = get_disks_url();

    // Reject where block size doesn't evenly divide total size (note that all
    // local storage disks have a block size of 4096)
    let error = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &disks_url)
            .body(Some(&params::DiskCreate::LocalStorage {
                identity: external::IdentityMetadataCreateParams {
                    name: "bad-disk".parse().unwrap(),
                    description: String::from("bad disk"),
                },

                size: external::ByteCount::try_from(
                    2 * MIN_DISK_SIZE_BYTES + 512,
                )
                .unwrap(),
            }))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap()
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(
        error.message,
        "unsupported value for \"size and block_size\": total size must be a multiple of block size 4096",
    );

    // Reject disks where the MIN_DISK_SIZE_BYTES doesn't evenly divide
    // the size
    let error = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &disks_url)
            .body(Some(&params::DiskCreate::LocalStorage {
                identity: external::IdentityMetadataCreateParams {
                    name: "bad-disk".parse().unwrap(),
                    description: String::from("bad disk"),
                },

                size: external::ByteCount::try_from(
                    2 * MIN_DISK_SIZE_BYTES + 4096,
                )
                .unwrap(),
            }))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap()
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(
        error.message,
        "unsupported value for \"size\": total size must be a multiple of 1 GiB",
    );
}

// Test creating a local storage disk larger than MAX_DISK_SIZE_BYTES
#[nexus_test]
async fn test_create_large_local_storage_disk(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;

    // Create some giant disks

    let mut disk_test = DiskTest::new(&cptestctx).await;

    for sled_agent in cptestctx.all_sled_agents() {
        disk_test
            .add_sized_zpool_with_datasets(
                sled_agent.sled_agent.id,
                7 * 1024, // 7 TiB
            )
            .await;
    }

    disk_test.propagate_datasets_to_sleds().await;

    create_project_and_pool(&client).await;

    let disks_url = get_disks_url();

    // A 5 TiB disk!
    let large_disk_size = external::ByteCount::from_gibibytes_u32(5 * 1024);

    assert!(large_disk_size.to_bytes() > MAX_DISK_SIZE_BYTES);

    NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &disks_url)
            .body(Some(&params::DiskCreate::LocalStorage {
                identity: external::IdentityMetadataCreateParams {
                    name: "chonk-disk".parse().unwrap(),
                    description: String::from("chonk"),
                },

                size: large_disk_size,
            }))
            .expect_status(Some(StatusCode::CREATED)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap()
    .parsed_body::<external::Disk>()
    .unwrap();
}

#[nexus_test]
async fn test_create_instance_with_local_storage(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let lockstep_client = &cptestctx.lockstep_client;
    let apictx = &cptestctx.server.server_context();
    let nexus = &apictx.nexus;
    let datastore = nexus.datastore();
    let opctx =
        OpContext::for_tests(cptestctx.logctx.log.new(o!()), datastore.clone());

    let instance_name = "bird-ecology";

    // Need the local storage datasets to be created! Enable blueprint
    // execution, then run through load + plan + execute + rendezvous table
    // create.

    nexus
        .blueprint_target_set_enabled(
            &opctx,
            BlueprintTargetSet {
                target_id: nexus
                    .blueprint_target_view(&opctx)
                    .await
                    .expect("current blueprint target")
                    .target_id,
                enabled: true,
            },
        )
        .await
        .expect("enable blueprint execution");

    run_blueprint_loader(lockstep_client).await;

    /*
    let status = run_blueprint_planner(lockstep_client).await;

    match status {
        BlueprintPlannerStatus::Targeted { .. } => {
            // ok
        }

        _ => {
            panic!("blueprint_planner status is {status:?}");
        }
    }
    */

    run_blueprint_executor(lockstep_client).await;

    run_blueprint_rendezvous(lockstep_client).await;

    // Assert local storage datasets were created.
    assert!(
        !datastore
            .local_storage_dataset_list_all_batched(&opctx)
            .await
            .unwrap()
            .is_empty()
    );

    create_project_and_pool(&client).await;
    let instance_url = get_instance_url(instance_name);

    // Create an instance with one local storage disk.
    let instance = nexus_test_utils::resource_helpers::create_instance_with(
        client,
        PROJECT_NAME,
        instance_name,
        &params::InstanceNetworkInterfaceAttachment::Default,
        vec![params::InstanceDiskAttachment::Create(params::DiskCreate {
            identity: IdentityMetadataCreateParams {
                name: "local-disk".parse().unwrap(),
                description: String::from("local disk"),
            },

            details: params::DiskCreateDetails::LocalStorage {
                block_size: params::BlockSize::try_from(4096).unwrap(),
            },

            size: ByteCount::try_from(2 * MIN_DISK_SIZE_BYTES).unwrap(),
        })],
        Vec::<params::ExternalIpCreate>::new(),
        true,
        Default::default(),
        None,
    )
    .await;

    let instance_id = InstanceUuid::from_untyped_uuid(instance.identity.id);

    // Poke the instance into an active state - this only works if instance
    // start worked.
    instance_simulate(nexus, &instance_id).await;

    let instance_next = instance_get(&client, &instance_url).await;
    assert_eq!(instance_next.runtime.run_state, InstanceState::Running);
}

#[nexus_test(extra_sled_agents = 1)]
async fn test_instance_no_migrate_with_local_storage(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    let lockstep_client = &cptestctx.lockstep_client;
    let apictx = &cptestctx.server.server_context();
    let nexus = &apictx.nexus;
    let instance_name = "bird-ecology";

    // Get the second sled to migrate to/from.
    let default_sled_id = cptestctx.first_sled_id();
    let other_sled_id = cptestctx.second_sled_id();

    create_project_and_pool(&client).await;
    let instance_url = get_instance_url(instance_name);

    // Create an instance with one local storage disk.
    let instance = nexus_test_utils::resource_helpers::create_instance_with(
        client,
        PROJECT_NAME,
        instance_name,
        &params::InstanceNetworkInterfaceAttachment::Default,
        vec![params::InstanceDiskAttachment::Create(params::DiskCreate {
            identity: IdentityMetadataCreateParams {
                name: "local-disk".parse().unwrap(),
                description: String::from("local disk"),
            },

            details: params::DiskCreateDetails::LocalStorage {
                block_size: params::BlockSize::try_from(4096).unwrap(),
            },

            size: ByteCount::try_from(2 * MIN_DISK_SIZE_BYTES).unwrap(),
        })],
        Vec::<params::ExternalIpCreate>::new(),
        true,
        Default::default(),
        None,
    )
    .await;
    let instance_id = InstanceUuid::from_untyped_uuid(instance.identity.id);

    // Poke the instance into an active state.
    instance_simulate(nexus, &instance_id).await;
    let instance_next = instance_get(&client, &instance_url).await;
    assert_eq!(instance_next.runtime.run_state, InstanceState::Running);

    let sled_info = nexus
        .active_instance_info(&instance_id, None)
        .await
        .unwrap()
        .expect("running instance should have a sled");

    let original_sled = sled_info.sled_id;
    let dst_sled_id = if original_sled == default_sled_id {
        other_sled_id
    } else {
        default_sled_id
    };

    // Attempt to migrate - this should fail.
    let migrate_url =
        format!("/instances/{}/migrate", &instance_id.to_string());

    let err = NexusRequest::new(
        RequestBuilder::new(lockstep_client, Method::POST, &migrate_url)
            .body(Some(&InstanceMigrateRequest { dst_sled_id }))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("request returns 400")
    .parsed_body::<HttpErrorResponseBody>()
    .expect("failed to parse error body");

    assert_eq!(
        err.message,
        format!("cannot migrate {} as it uses local storage", instance_id),
    );
}
