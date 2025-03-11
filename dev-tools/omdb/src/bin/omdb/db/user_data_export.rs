// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `omdb db user-data-export` subcommands

use crate::Omdb;
use crate::check_allow_destructive::DestructiveOperationToken;
use async_bb8_diesel::AsyncRunQueryDsl;
use clap::Args;
use clap::Subcommand;
use diesel::prelude::*;
use nexus_db_model::UserDataExportRecord;
use nexus_db_model::UserDataExportResourceType;
use nexus_db_model::to_db_typed_uuid;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db::DataStore;
use omicron_uuid_kinds::UserDataExportUuid;
use omicron_uuid_kinds::VolumeUuid;
use std::net::SocketAddrV6;
use tabled::Tabled;
use uuid::Uuid;

/// `omdb db user-data-export` subcommand
#[derive(Debug, Args, Clone)]
pub struct UserDataExportArgs {
    #[command(subcommand)]
    command: UserDataExportCommands,
}

#[derive(Debug, Subcommand, Clone)]
enum UserDataExportCommands {
    /// Check if a read-only resource has a user data export object
    Query(UserDataExportQueryArgs),

    /// Manually request that a user data export object be deleted.
    Delete(UserDataExportDeleteArgs),

    /// Show the user data export related changes required by Nexus
    ShowChangeset,
}

#[derive(Clone, Debug, Args)]
struct UserDataExportQueryArgs {
    #[clap(long)]
    resource_type: UserDataExportResourceType,

    #[clap(long)]
    resource_id: Uuid,
}

#[derive(Clone, Debug, Args)]
struct UserDataExportDeleteArgs {
    #[clap(long)]
    user_data_export_id: UserDataExportUuid,
}

impl UserDataExportArgs {
    pub async fn exec(
        &self,
        omdb: &Omdb,
        opctx: &OpContext,
        datastore: &DataStore,
    ) -> Result<(), anyhow::Error> {
        match &self.command {
            UserDataExportCommands::Query(args) => {
                cmd_user_data_export_query(opctx, datastore, args).await
            }

            UserDataExportCommands::Delete(args) => {
                let token = omdb.check_allow_destructive()?;

                cmd_user_data_export_delete(opctx, datastore, args, token).await
            }

            UserDataExportCommands::ShowChangeset => {
                cmd_user_data_export_show_changeset(opctx, datastore).await
            }
        }
    }
}

async fn cmd_user_data_export_query(
    _opctx: &OpContext,
    datastore: &DataStore,
    args: &UserDataExportQueryArgs,
) -> Result<(), anyhow::Error> {
    let conn = datastore.pool_connection_for_tests().await?;

    let record = {
        use nexus_db_schema::schema::user_data_export::dsl;

        dsl::user_data_export
            .filter(dsl::resource_type.eq(args.resource_type))
            .filter(dsl::resource_id.eq(args.resource_id))
            .select(UserDataExportRecord::as_select())
            .first_async(&*conn)
            .await?
    };

    #[derive(Tabled)]
    struct Row {
        id: UserDataExportUuid,
        resource_type: String,
        resource_id: Uuid,
        resource_deleted: bool,
        pantry_address: SocketAddrV6,
        volume_id: VolumeUuid,
    }

    let rows: Vec<_> = vec![Row {
        id: record.id(),
        resource_type: args.resource_type.to_string(),
        resource_id: args.resource_id,
        resource_deleted: record.deleted(),
        pantry_address: record.pantry_address(),
        volume_id: record.volume_id(),
    }];

    let table = tabled::Table::new(rows)
        .with(tabled::settings::Style::psql())
        .to_string();

    println!("{}", table);

    Ok(())
}

async fn cmd_user_data_export_delete(
    _opctx: &OpContext,
    datastore: &DataStore,
    args: &UserDataExportDeleteArgs,
    _destruction_token: DestructiveOperationToken,
) -> Result<(), anyhow::Error> {
    let conn = datastore.pool_connection_for_tests().await?;

    {
        use nexus_db_schema::schema::user_data_export::dsl;

        diesel::update(dsl::user_data_export)
            .filter(dsl::id.eq(to_db_typed_uuid(args.user_data_export_id)))
            .set(dsl::resource_deleted.eq(true))
            .execute_async(&*conn)
            .await?;
    }

    println!("marked record {} for deletion", args.user_data_export_id);

    Ok(())
}

async fn cmd_user_data_export_show_changeset(
    opctx: &OpContext,
    datastore: &DataStore,
) -> Result<(), anyhow::Error> {
    let changeset = datastore.user_data_export_changeset(opctx).await?;

    #[derive(Tabled)]
    struct CreateRow {
        action: String,
        resource_type: String,
        resource_id: Uuid,
    }

    #[derive(Tabled)]
    struct DeleteRow {
        action: String,
        id: UserDataExportUuid,
        resource_type: String,
        resource_id: Uuid,
        resource_deleted: bool,
        pantry_address: SocketAddrV6,
        volume_id: VolumeUuid,
    }

    let create_rows: Vec<_> = changeset
        .create_required
        .iter()
        .map(|resource| CreateRow {
            action: String::from("create"),
            resource_type: resource.type_string(),
            resource_id: resource.id(),
        })
        .collect();

    let delete_rows: Vec<_> = changeset
        .delete_required
        .iter()
        .map(|record| DeleteRow {
            action: String::from("delete"),
            id: record.id(),
            resource_type: record.resource().type_string(),
            resource_id: record.resource().id(),
            resource_deleted: record.deleted(),
            pantry_address: record.pantry_address(),
            volume_id: record.volume_id(),
        })
        .collect();

    let table = tabled::Table::new(create_rows)
        .with(tabled::settings::Style::psql())
        .to_string();

    println!("{}", table);

    let table = tabled::Table::new(delete_rows)
        .with(tabled::settings::Style::psql())
        .to_string();

    println!("{}", table);

    Ok(())
}
