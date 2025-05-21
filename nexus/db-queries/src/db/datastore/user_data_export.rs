// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DataStore`] methods on [`UserDataExportRecord`]s.

use super::DataStore;
use crate::authz;
use crate::context::OpContext;
use crate::db;
use nexus_db_lookup::DbConnection;
use nexus_db_errors::public_error_from_diesel;
use nexus_db_errors::ErrorHandler;
use nexus_db_errors::OptionalError;
use crate::db::model::to_db_typed_uuid;
use crate::db::model::UserDataExportRecord;
use crate::db::model::UserDataExportResource;
use crate::db::model::UserDataExportResourceType;
use crate::db::model::Snapshot;
use crate::db::model::Image;
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
use omicron_uuid_kinds::UserDataExportUuid;
use uuid::Uuid;
use std::net::SocketAddrV6;
use nexus_auth::authz::ApiResource;

impl DataStore {
    async fn user_data_export_create_in_txn(
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<Error>,
        id: UserDataExportUuid,
        resource: UserDataExportResource,
        pantry_address: SocketAddrV6,
        volume_id: VolumeUuid,
    ) -> Result<UserDataExportRecord, diesel::result::Error> {
        let user_data_export = UserDataExportRecord::new(
            id,
            resource,
            pantry_address,
            volume_id,
        );

        use nexus_db_schema::schema::user_data_export::dsl;

        // Has an export with this id been created already? If so,
        // return that.
        let existing_export: Option<UserDataExportRecord> =
            dsl::user_data_export
                .filter(dsl::id.eq(to_db_typed_uuid(id)))
                .select(UserDataExportRecord::as_select())
                .first_async(conn)
                .await
                .optional()?;

        if let Some(existing_export) = existing_export {
            return Ok(existing_export);
        }

        // Does the resource being referenced still exist?
        let resource_id: Uuid = match resource {
            UserDataExportResource::Snapshot { id } => {
                use nexus_db_schema::schema::snapshot::dsl as snapshot_dsl;

                let snapshot: Option<Snapshot> = snapshot_dsl::snapshot
                    .filter(snapshot_dsl::id.eq(id))
                    .select(Snapshot::as_select())
                    .first_async(conn)
                    .await
                    .optional()?;

                let still_here = match snapshot {
                    Some(snapshot) => snapshot.time_deleted().is_none(),
                    None => false
                };

                if !still_here {
                    return Err(err.bail(Error::non_resourcetype_not_found(
                        format!("snapshot with id {id} not found or deleted")
                    )));
                }

                id
            }

            UserDataExportResource::Image { id } => {
                use nexus_db_schema::schema::image::dsl as image_dsl;

                let image: Option<Image> = image_dsl::image
                    .filter(image_dsl::id.eq(id))
                    .select(Image::as_select())
                    .first_async(conn)
                    .await
                    .optional()?;

                let still_here = match image {
                    Some(image) => image.time_deleted().is_none(),
                    None => false
                };

                if !still_here {
                    return Err(err.bail(Error::non_resourcetype_not_found(
                        format!("image with id {id} not found or deleted")
                    )));
                }

                id
            }
        };

        // Does an export object for this resource exist?
        let existing_export: Option<UserDataExportRecord> =
            dsl::user_data_export
                .filter(dsl::resource_id.eq(resource_id))
                .select(UserDataExportRecord::as_select())
                .first_async(conn)
                .await
                .optional()?;

        if existing_export.is_some() {
            return Err(err.bail(Error::conflict(
                "export already started for resource"
            )));
        }

        // Otherwise, insert the new export object
        let rows_inserted = diesel::insert_into(dsl::user_data_export)
            .values(user_data_export.clone())
            .execute_async(conn)
            .await?;

        if rows_inserted != 1 {
            return Err(err.bail(Error::internal_error(&format!(
                "{rows_inserted} rows inserted!"
            ))));
        }

        Ok(user_data_export)
    }

    pub async fn user_data_export_create_for_snapshot(
        &self,
        opctx: &OpContext,
        id: UserDataExportUuid,
        authz_snapshot: &authz::Snapshot,
        pantry_address: SocketAddrV6,
        volume_id: VolumeUuid,
    ) -> CreateResult<UserDataExportRecord> {
        let err = OptionalError::new();
        let conn = self.pool_connection_authorized(opctx).await?;

        self
            .transaction_retry_wrapper("user_data_export_create")
            .transaction(&conn, |conn| {
                let err = err.clone();
                async move {
                    Self::user_data_export_create_in_txn(
                        &conn,
                        err,
                        id,
                        UserDataExportResource::Snapshot {
                            id: authz_snapshot.id(),
                        },
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

    pub async fn user_data_export_create_for_image(
        &self,
        opctx: &OpContext,
        id: UserDataExportUuid,
        authz_image: &authz::Image,
        pantry_address: SocketAddrV6,
        volume_id: VolumeUuid,
    ) -> CreateResult<UserDataExportRecord> {
        let err = OptionalError::new();
        let conn = self.pool_connection_authorized(opctx).await?;

        self
            .transaction_retry_wrapper("user_data_export_create")
            .transaction(&conn, |conn| {
                let err = err.clone();
                async move {
                    Self::user_data_export_create_in_txn(
                        &conn,
                        err,
                        id,
                        UserDataExportResource::Image {
                            id: authz_image.id(),
                        },
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

    pub async fn user_data_export_delete(
        &self,
        opctx: &OpContext,
        id: UserDataExportUuid,
    ) -> DeleteResult {
        let conn = self.pool_connection_authorized(opctx).await?;

        use nexus_db_schema::schema::user_data_export::dsl;

        diesel::delete(dsl::user_data_export)
            .filter(dsl::id.eq(to_db_typed_uuid(id)))
            .execute_async(&*conn)
            .await
            .map(|_| ())
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    pub async fn user_data_export_lookup_for_snapshot(
        &self,
        opctx: &OpContext,
        authz_snapshot: &authz::Snapshot,
    ) -> LookupResult<Option<UserDataExportRecord>> {
        let conn = self.pool_connection_authorized(opctx).await?;

        use nexus_db_schema::schema::user_data_export::dsl;

        dsl::user_data_export
            .filter(dsl::resource_type.eq(UserDataExportResourceType::Snapshot))
            .filter(dsl::resource_id.eq(authz_snapshot.id()))
            .first_async(&*conn)
            .await
            .optional()
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    // XXX resources needing export object create

    // XXX resources needing export object delete
}

// XXX tests

// XXX resource id collision

