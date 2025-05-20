// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// XXX copyright?

//! XXX TODO

use std::sync::Arc;
use internal_dns_types::names::ServiceName;
use nexus_types::identity::Resource;
use nexus_db_queries::authn;
use nexus_db_queries::authz;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db;
use nexus_db_lookup::LookupPath;
use nexus_db_lookup::lookup;
use nexus_types::external_api::params;
use nexus_types::external_api::params::SnapshotSelector;
use omicron_common::api::external::http_pagination::PaginatedBy;
use omicron_common::api::external::CreateResult;
use omicron_common::api::external::DeleteResult;
use omicron_common::api::external::Error;
use omicron_common::api::external::ListResultVec;
use omicron_common::api::external::LookupResult;
use omicron_common::api::external::NameOrId;
use super::sagas::user_data_export_create;
use super::sagas::user_data_export_delete;

impl super::Nexus {
    pub(crate) async fn user_data_export(
        self: &Arc<Self>,
        opctx: &OpContext,
        snapshot_lookup: &lookup::Snapshot<'_>,
        param: params::ExportBlocksBulkReadRequest,
    ) -> LookupResult<params::ExportBlocksBulkReadResponse> {
        let (.., authz_snapshot, _) = snapshot_lookup.fetch_for(authz::Action::Read).await?;

        let user_data_export = match self.db_datastore.user_data_export_lookup(
            opctx,
            &authz_snapshot,
        )
        .await? {
            Some(user_data_export) => user_data_export,

            None => {
                let saga_params = user_data_export_create::Params {
                    serialized_authn: authn::saga::Serialized::for_opctx(opctx),
                    snapshot_id: authz_snapshot.id(),
                };

                self
                    .sagas
                    .saga_execute::<user_data_export_create::SagaUserDataExportCreate>(
                        saga_params,
                    )
                    .await?;

                let Some(user_data_export) = self.db_datastore.user_data_export_lookup(
                    opctx,
                    &authz_snapshot,
                )
                .await? else {
                    return Err(Error::Gone);
                };

                user_data_export
            }
        };

        let pantry_address = user_data_export.pantry_address();

        // Check if it's gone from DNS, and fail early if so
        if self.is_internal_service_gone(
            ServiceName::CruciblePantry,
            pantry_address,
        ).await? {
            warn!(self.log, "pantry {pantry_address} is gone from DNS!");
            return Err(Error::Gone);
        }

        info!(
            self.log,
            "bulk read of {} bytes from offset {} of snapshot {} using pantry \
            {pantry_address}",
            param.size,
            param.offset,
            authz_snapshot.id(),
        );

        let client = crucible_pantry_client::Client::new_with_client(
            &format!("http://{}", pantry_address),
            self.reqwest_client.clone(),
        );

        let request = crucible_pantry_client::types::BulkReadRequest {
            offset: param.offset,
            size: param.size,
        };

        let response = client
            .bulk_read(&user_data_export.volume_id().to_string(), &request)
            .await
            .map_err(|e| match e {
                crucible_pantry_client::Error::ErrorResponse(rv) => {
                    match rv.status() {
                        status if status.is_client_error() => {
                            Error::invalid_request(&rv.message)
                        }

                        _ => Error::internal_error(&rv.message),
                    }
                }

                _ => Error::internal_error(&format!(
                    "error sending bulk read to pantry: {e}"
                )),
            })?;

        let crucible_pantry_client::types::BulkReadResponse {
            base64_encoded_data
        } = response.into_inner();

        return Ok(params::ExportBlocksBulkReadResponse { base64_encoded_data });
    }

    // XXX not required, wrap up in snapshot delete saga
    pub(crate) async fn user_data_export_delete(
        self: &Arc<Self>,
        opctx: &OpContext,
        snapshot_lookup: &lookup::Snapshot<'_>,
    ) -> DeleteResult {
        let (.., authz_snapshot, _) = snapshot_lookup.fetch_for(authz::Action::Delete).await?;

        let Some(user_data_export) = self.db_datastore.user_data_export_lookup(
                opctx,
                &authz_snapshot,
            )
            .await?
        else {
            return Err(Error::Gone);
        };

        let saga_params = user_data_export_delete::Params {
            serialized_authn: authn::saga::Serialized::for_opctx(opctx),
            snapshot_id: authz_snapshot.id(),
            user_data_export_id: user_data_export.id(),
            volume_id: user_data_export.volume_id(),
        };

        self.sagas
            .saga_execute::<user_data_export_delete::SagaUserDataExportDelete>(
                saga_params,
            )
            .await?;

        Ok(())
    }
}
