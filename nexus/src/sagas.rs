// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*!
 * Saga actions, undo actions, and saga constructors used in Nexus.
 */

/*
 * NOTE: We want to be careful about what interfaces we expose to saga actions.
 * In the future, we expect to mock these out for comprehensive testing of
 * correctness, idempotence, etc.  The more constrained this interface is, the
 * easier it will be to test, version, and update in deployed systems.
 */

use crate::db;
use crate::db::identity::Resource;
use crate::external_api::params;
use crate::saga_interface::SagaContext;
use chrono::Utc;
use lazy_static::lazy_static;
use omicron_common::api::external::Generation;
use omicron_common::api::external::InstanceState;
use omicron_common::api::external::Name;
use omicron_common::api::internal::nexus::InstanceRuntimeState;
use omicron_common::api::internal::sled_agent::InstanceHardware;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use steno::new_action_noop_undo;
use steno::ActionContext;
use steno::ActionError;
use steno::SagaTemplate;
use steno::SagaTemplateBuilder;
use steno::SagaTemplateGeneric;
use steno::SagaType;
use uuid::Uuid;

/*
 * We'll need a richer mechanism for registering sagas, but this works for now.
 */
pub const SAGA_INSTANCE_CREATE_NAME: &'static str = "instance-create";
pub const SAGA_INSTANCE_MIGRATE_NAME: &'static str = "instance-migrate";
pub const SAGA_INSTANCE_MIGRATE_INPLACE_NAME: &'static str =
    "instance-migrate-inplace";
lazy_static! {
    pub static ref SAGA_INSTANCE_CREATE_TEMPLATE: Arc<SagaTemplate<SagaInstanceCreate>> =
        Arc::new(saga_instance_create());
    pub static ref SAGA_INSTANCE_MIGRATE_TEMPLATE: Arc<SagaTemplate<SagaInstanceMigrate>> =
        Arc::new(saga_instance_migrate(MigrateType::Sled));
    pub static ref SAGA_INSTANCE_MIGRATE_INPLACE_TEMPLATE: Arc<SagaTemplate<SagaInstanceMigrate>> =
        Arc::new(saga_instance_migrate(MigrateType::InPlace));
}

lazy_static! {
    pub static ref ALL_TEMPLATES: BTreeMap<&'static str, Arc<dyn SagaTemplateGeneric<Arc<SagaContext>>>> =
        all_templates();
}

fn all_templates(
) -> BTreeMap<&'static str, Arc<dyn SagaTemplateGeneric<Arc<SagaContext>>>> {
    vec![
        (
            SAGA_INSTANCE_CREATE_NAME,
            Arc::clone(&SAGA_INSTANCE_CREATE_TEMPLATE)
                as Arc<dyn SagaTemplateGeneric<Arc<SagaContext>>>,
        ),
        (
            SAGA_INSTANCE_MIGRATE_NAME,
            Arc::clone(&SAGA_INSTANCE_MIGRATE_TEMPLATE)
                as Arc<dyn SagaTemplateGeneric<Arc<SagaContext>>>,
        ),
        (
            SAGA_INSTANCE_MIGRATE_INPLACE_NAME,
            Arc::clone(&SAGA_INSTANCE_MIGRATE_INPLACE_TEMPLATE)
                as Arc<dyn SagaTemplateGeneric<Arc<SagaContext>>>,
        ),
    ]
    .into_iter()
    .collect()
}

async fn saga_generate_uuid<UserType: SagaType>(
    _: ActionContext<UserType>,
) -> Result<Uuid, ActionError> {
    Ok(Uuid::new_v4())
}

/*
 * "Create Instance" saga template
 */

#[derive(Debug, Deserialize, Serialize)]
pub struct ParamsInstanceCreate {
    pub project_id: Uuid,
    pub create_params: params::InstanceCreate,
}

#[derive(Debug)]
pub struct SagaInstanceCreate;
impl SagaType for SagaInstanceCreate {
    type SagaParamsType = Arc<ParamsInstanceCreate>;
    type ExecContextType = Arc<SagaContext>;
}

pub fn saga_instance_create() -> SagaTemplate<SagaInstanceCreate> {
    let mut template_builder = SagaTemplateBuilder::new();

    template_builder.append(
        "instance_id",
        "GenerateInstanceId",
        new_action_noop_undo(saga_generate_uuid),
    );

    template_builder.append(
        "propolis_id",
        "GeneratePropolisId",
        new_action_noop_undo(saga_generate_uuid),
    );

    template_builder.append(
        "server_id",
        "AllocServer",
        // TODO-robustness This still needs an undo action, and we should really
        // keep track of resources and reservations, etc.  See the comment on
        // SagaContext::alloc_server()
        new_action_noop_undo(sic_alloc_server),
    );

    template_builder.append(
        "initial_runtime",
        "CreateInstanceRecord",
        new_action_noop_undo(sic_create_instance_record),
    );

    template_builder.append(
        "instance_ensure",
        "InstanceEnsure",
        new_action_noop_undo(sic_instance_ensure),
    );

    template_builder.build()
}

async fn sic_alloc_server(
    sagactx: ActionContext<SagaInstanceCreate>,
) -> Result<Uuid, ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params();
    osagactx
        .alloc_server(&params.create_params)
        .await
        .map_err(ActionError::action_failed)
}

async fn sic_create_instance_record(
    sagactx: ActionContext<SagaInstanceCreate>,
) -> Result<InstanceHardware, ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params();
    let sled_uuid = sagactx.lookup::<Uuid>("server_id");
    let instance_id = sagactx.lookup::<Uuid>("instance_id");
    let propolis_uuid = sagactx.lookup::<Uuid>("propolis_id");

    let runtime = InstanceRuntimeState {
        run_state: InstanceState::Creating,
        sled_uuid: sled_uuid?,
        propolis_uuid: propolis_uuid?,
        propolis_addr: None,
        hostname: params.create_params.hostname.clone(),
        memory: params.create_params.memory,
        ncpus: params.create_params.ncpus,
        gen: Generation::new(),
        time_updated: Utc::now(),
    };

    let new_instance = db::model::Instance::new(
        instance_id?,
        params.project_id,
        &params.create_params,
        runtime.into(),
    );

    let instance = osagactx
        .datastore()
        .project_create_instance(new_instance)
        .await
        .map_err(ActionError::action_failed)?;

    // TODO: Populate this with an appropriate NIC.
    // See also: instance_set_runtime in nexus.rs for a similar construction.
    Ok(InstanceHardware {
        runtime: instance.runtime().clone().into(),
        nics: vec![],
    })
}

async fn sic_instance_ensure(
    sagactx: ActionContext<SagaInstanceCreate>,
) -> Result<(), ActionError> {
    /*
     * TODO-correctness is this idempotent?
     */
    let osagactx = sagactx.user_data();
    let runtime_params =
        sled_agent_client::types::InstanceRuntimeStateRequested {
            run_state:
                sled_agent_client::types::InstanceStateRequested::Running,
        };
    let instance_id = sagactx.lookup::<Uuid>("instance_id")?;
    let sled_uuid = sagactx.lookup::<Uuid>("server_id")?;
    let initial_runtime =
        sagactx.lookup::<InstanceHardware>("initial_runtime")?;
    let sa = osagactx
        .sled_client(&sled_uuid)
        .await
        .map_err(ActionError::action_failed)?;

    /*
     * Ask the sled agent to begin the state change.  Then update the database
     * to reflect the new intermediate state.  If this update is not the newest
     * one, that's fine.  That might just mean the sled agent beat us to it.
     */
    let new_runtime_state = sa
        .instance_put(
            &instance_id,
            &sled_agent_client::types::InstanceEnsureBody {
                initial: sled_agent_client::types::InstanceHardware::from(
                    initial_runtime,
                ),
                target: runtime_params,
                migrate: None,
            },
        )
        .await
        .map_err(omicron_common::api::external::Error::from)
        .map_err(ActionError::action_failed)?;

    let new_runtime_state: InstanceRuntimeState = new_runtime_state.into();

    osagactx
        .datastore()
        .instance_update_runtime(&instance_id, &new_runtime_state.into())
        .await
        .map(|_| ())
        .map_err(ActionError::action_failed)
}

/*
 * "Migrate Instance" saga template
 */
#[derive(Debug, Deserialize, Serialize)]
pub struct ParamsInstanceMigrate {
    // TODO design: We can't just use db::model::Instance here as it doesn't impl
    // Serialize so instead we need the project_id/instance_name to look it up
    // in the saga.
    pub project_id: Uuid,
    pub instance_name: Name,
    pub migrate_params: params::InstanceMigrate,
}

#[derive(Debug)]
pub struct SagaInstanceMigrate;
impl SagaType for SagaInstanceMigrate {
    type SagaParamsType = Arc<ParamsInstanceMigrate>;
    type ExecContextType = Arc<SagaContext>;
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
pub enum MigrateType {
    /// Migrate to a new propolis instance on a different sled
    Sled,
    /// Migrate to a new propolis instance on the same sled
    InPlace,
}

pub fn saga_instance_migrate(
    migrate_type: MigrateType,
) -> SagaTemplate<SagaInstanceMigrate> {
    let mut template_builder = SagaTemplateBuilder::new();

    template_builder.append(
        "migrate_type",
        "MigrateType",
        new_action_noop_undo(move |_| futures::future::ok(migrate_type)),
    );

    template_builder.append(
        "dst_propolis_id",
        "GeneratePropolisId",
        new_action_noop_undo(saga_generate_uuid),
    );

    // Make sure we have a sled allocated for the destination
    template_builder.append(
        "dst_server_id",
        "AllocServer",
        // TODO robustness This still needs an undo action, and we should really
        // keep track of resources and reservations, etc. See the comment on
        // SagaContext::alloc_server()
        new_action_noop_undo(sim_alloc_server),
    );

    template_builder.append(
        "instance_migrate",
        "InstanceMigrate",
        // TODO robustness: This needs an undo action
        new_action_noop_undo(sim_instance_migrate),
    );

    template_builder.append(
        "cleanup_source",
        "CleanupSource",
        // TODO robustness: This needs an undo action. Is it even possible
        // to undo at this point?
        new_action_noop_undo(sim_cleanup_source),
    );

    template_builder.build()
}

async fn sim_alloc_server(
    sagactx: ActionContext<SagaInstanceMigrate>,
) -> Result<Uuid, ActionError> {
    use omicron_common::api::external::{
        ByteCount, IdentityMetadataCreateParams, InstanceCpuCount,
    };

    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params();

    let migrate_type = sagactx.lookup::<MigrateType>("migrate_type")?;

    if let MigrateType::InPlace = migrate_type {
        Ok(params.migrate_params.dst_sled_uuid)
    } else {
        // TODO: These parameters are unused, `alloc_server` just
        // needs to be passed something. See comment on
        // SagaContext::alloc_server()
        let create_params = params::InstanceCreate {
            identity: IdentityMetadataCreateParams {
                name: "unused".parse().unwrap(),
                description: "unused".to_string(),
            },
            ncpus: InstanceCpuCount(0),
            memory: ByteCount::from_mebibytes_u32(0),
            hostname: "unused".to_string(),
        };
        osagactx
            .alloc_server(&create_params)
            .await
            .map_err(ActionError::action_failed)
    }
}

async fn sim_instance_migrate(
    sagactx: ActionContext<SagaInstanceMigrate>,
) -> Result<(), ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params();

    let dst_sled_uuid = sagactx.lookup("dst_server_id")?;
    let dst_propolis_uuid = sagactx.lookup::<Uuid>("dst_propolis_id")?;

    let instance = osagactx
        .datastore()
        .instance_fetch_by_name(
            &params.project_id,
            &params.instance_name.clone().into(),
        )
        .await
        .map_err(ActionError::action_failed)?;

    let instance_id = instance.id();

    let old_runtime: InstanceRuntimeState = instance.runtime_state.into();
    let runtime = InstanceRuntimeState {
        sled_uuid: dst_sled_uuid,
        propolis_uuid: dst_propolis_uuid,
        propolis_addr: None,
        ..old_runtime
    };
    let instance_hardware = sled_agent_client::types::InstanceHardware {
        runtime: runtime.into(),
        // TODO: populate NICs
        nics: vec![],
    };
    let target = sled_agent_client::types::InstanceRuntimeStateRequested {
        // TODO: is this the appropriate state?
        run_state: sled_agent_client::types::InstanceStateRequested::Running,
    };

    let src_propolis_uuid = old_runtime.propolis_uuid;
    let src_propolis_addr = old_runtime.propolis_addr.ok_or_else(|| {
        ActionError::action_failed("expected source propolis-addr".to_string())
    })?;

    let dst_sa = osagactx
        .sled_client(&dst_sled_uuid)
        .await
        .map_err(ActionError::action_failed)?;

    let new_runtime_state: InstanceRuntimeState = dst_sa
        .instance_put(
            &instance_id,
            &sled_agent_client::types::InstanceEnsureBody {
                initial: instance_hardware,
                target,
                migrate: Some(omicron_common::api::internal::sled_agent::InstanceMigrateParams {
                    src_propolis_addr,
                    src_propolis_uuid,
                }.into()),
            },
        )
        .await
        .map_err(omicron_common::api::external::Error::from)
        .map_err(ActionError::action_failed)?
        .into();

    osagactx
        .datastore()
        .instance_update_runtime(&instance_id, &new_runtime_state.into())
        .await
        .map_err(ActionError::action_failed)?;

    Ok(())
}

async fn sim_cleanup_source(
    _sagactx: ActionContext<SagaInstanceMigrate>,
) -> Result<(), ActionError> {
    // TODO: clean up the previous instance whether it's on the same sled or a different one
    Ok(())
}
