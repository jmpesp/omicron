// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DataStore`] methods on [`SnapshotExport`]s.

use super::DataStore;
use crate::authz;
use crate::context::OpContext;
use crate::db;
use crate::db::DbConnection;
use crate::db::error::public_error_from_diesel;
use crate::db::error::ErrorHandler;
use crate::db::model::to_db_typed_uuid;
use crate::db::model::SnapshotExport;
use crate::db::model::Snapshot;
use crate::transaction_retry::OptionalError;
use async_bb8_diesel::AsyncRunQueryDsl;
use chrono::Utc;
use diesel::prelude::*;
use diesel::OptionalExtension;
use nexus_types::identity::Resource;
use omicron_common::api::external::CreateResult;
use omicron_common::api::external::Error;
use omicron_common::api::external::LookupResult;
use omicron_common::api::external::DeleteResult;
use omicron_uuid_kinds::VolumeUuid;
use omicron_uuid_kinds::SnapshotExportUuid;
use uuid::Uuid;
use std::net::SocketAddrV6;
use nexus_auth::authz::ApiResource;

impl DataStore {
    async fn snapshot_export_create_in_txn(
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<Error>,
        authz_snapshot: &authz::Snapshot,
        id: SnapshotExportUuid,
        pantry_address: SocketAddrV6,
        volume_id: VolumeUuid,
    ) -> Result<SnapshotExport, diesel::result::Error> {
        let snapshot_export = SnapshotExport::new(id, authz_snapshot.id(), pantry_address, volume_id);

        use nexus_db_schema::schema::snapshot_export::dsl;
        use nexus_db_schema::schema::snapshot::dsl as snapshot_dsl;

        // Has an export with this id been created already? If so,
        // return that.
        let existing_export: Option<SnapshotExport> = dsl::snapshot_export
            .filter(dsl::id.eq(to_db_typed_uuid(id)))
            .select(SnapshotExport::as_select())
            .first_async(conn)
            .await
            .optional()?;

        if let Some(existing_export) = existing_export {
            return Ok(existing_export);
        }

        // Does the snapshot being referenced still exist?
        let snapshot: Option<Snapshot> = snapshot_dsl::snapshot
            .filter(snapshot_dsl::id.eq(authz_snapshot.id()))
            .select(Snapshot::as_select())
            .first_async(conn)
            .await
            .optional()?;

        let still_here = match snapshot {
            Some(snapshot) => snapshot.time_deleted().is_none(),
            None => false
        };

        if !still_here {
            return Err(err.bail(authz_snapshot.not_found()));
        }

        // Does an export object for this snapshot exist?
        let existing_export: Option<SnapshotExport> = dsl::snapshot_export
            .filter(dsl::snapshot_id.eq(authz_snapshot.id()))
            .select(SnapshotExport::as_select())
            .first_async(conn)
            .await
            .optional()?;

        if existing_export.is_some() {
            return Err(err.bail(Error::conflict(
                "export already started for snapshot"
            )));
        }

        // Otherwise, insert the new export object
        let rows_inserted = diesel::insert_into(dsl::snapshot_export)
            .values(snapshot_export.clone())
            .execute_async(conn)
            .await?;

        if rows_inserted != 1 {
            return Err(err.bail(Error::internal_error(&format!(
                "{rows_inserted} rows inserted!"
            ))));
        }

        Ok(snapshot_export)
    }

    pub async fn snapshot_export_create(
        &self,
        opctx: &OpContext,
        authz_snapshot: &authz::Snapshot,
        id: SnapshotExportUuid,
        pantry_address: SocketAddrV6,
        volume_id: VolumeUuid,
    ) -> CreateResult<SnapshotExport> {
        opctx.authorize(authz::Action::Read, authz_snapshot).await?;

        let err = OptionalError::new();
        let conn = self.pool_connection_authorized(opctx).await?;

        self
            .transaction_retry_wrapper("snapshot_export_create")
            .transaction(&conn, |conn| {
                let err = err.clone();
                async move {
                    Self::snapshot_export_create_in_txn(
                        &conn,
                        err,
                        authz_snapshot,
                        id,
                        pantry_address,
                        volume_id,
                    )
                    .await
                }
            })
            .await
            .map_err(|e| {
                if let Some(err) = err.take() {
                    err
                } else {
                    public_error_from_diesel(e, ErrorHandler::Server)
                }
            })
    }

    pub async fn snapshot_export_delete(
        &self,
        opctx: &OpContext,
        authz_snapshot: &authz::Snapshot,
        id: SnapshotExportUuid,
    ) -> DeleteResult {
        opctx.authorize(authz::Action::Delete, authz_snapshot).await?;

        let conn = self.pool_connection_authorized(opctx).await?;

        use nexus_db_schema::schema::snapshot_export::dsl;

        diesel::delete(dsl::snapshot_export)
            .filter(dsl::id.eq(to_db_typed_uuid(id)))
            .execute_async(&*conn)
            .await
            .map(|_| ())
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    pub async fn snapshot_export_lookup(
        &self,
        opctx: &OpContext,
        authz_snapshot: &authz::Snapshot,
    ) -> LookupResult<Option<SnapshotExport>> {
        opctx.authorize(authz::Action::Read, authz_snapshot).await?;

        let conn = self.pool_connection_authorized(opctx).await?;

        use nexus_db_schema::schema::snapshot_export::dsl;

        dsl::snapshot_export
            .filter(dsl::snapshot_id.eq(authz_snapshot.id()))
            .first_async(&*conn)
            .await
            .optional()
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }
}

// XXX tests

