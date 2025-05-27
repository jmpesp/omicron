// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// XXX copyright?

//! XXX TODO

use super::sagas::user_data_export_create;
use super::sagas::user_data_export_delete;
use bytes::Bytes;
use dropshot::Body;
use dropshot::ErrorStatusCode;
use dropshot::HttpError;
use http::{Response, StatusCode, header};
use http_body_util::channel::Channel;
use internal_dns_types::names::ServiceName;
use nexus_db_lookup::LookupPath;
use nexus_db_lookup::lookup;
use nexus_db_queries::authn;
use nexus_db_queries::authz;
use nexus_db_queries::context::OpContext;
use nexus_db_queries::db;
use nexus_types::external_api::params;
use nexus_types::external_api::params::SnapshotSelector;
use nexus_types::identity::Resource;
use omicron_common::api::external::CreateResult;
use omicron_common::api::external::DeleteResult;
use omicron_common::api::external::Error;
use omicron_common::api::external::ListResultVec;
use omicron_common::api::external::LookupResult;
use omicron_common::api::external::NameOrId;
use omicron_common::api::external::http_pagination::PaginatedBy;
use omicron_uuid_kinds::VolumeUuid;
use range_requests::PotentialRange;
use range_requests::SingleRange;
use slog::Logger;
use std::net::SocketAddrV6;
use std::sync::Arc;
use uuid::Uuid;

async fn snapshot_blocks_read_task(
    mut sender: http_body_util::channel::Sender<Bytes>,
    log: Logger,
    client: reqwest::Client,
    snapshot_id: Uuid,
    snapshot_size: u64,
    pantry_address: SocketAddrV6,
    maybe_range: Option<SingleRange>,
    volume_id: VolumeUuid,
) {
    let range_start: u64;
    let range_end: u64;

    (range_start, range_end) = match maybe_range {
        Some(range) => (range.start(), range.end_inclusive() + 1),
        None => (0, snapshot_size),
    };

    info!(
        log,
        "read of snapshot {} start {} end {} using pantry {}",
        snapshot_id,
        range_start,
        range_end,
        pantry_address,
    );

    let client = crucible_pantry_client::Client::new_with_client(
        &format!("http://{}", pantry_address),
        client,
    );

    const PANTRY_MAX_CHUNK_SIZE: u64 = 512 * 1024;

    let mut offset: u64 = range_start;

    while (offset < range_end) {
        let end = std::cmp::min(offset + PANTRY_MAX_CHUNK_SIZE, range_end);
        let size: u64 = (end - offset);

        assert!(size <= PANTRY_MAX_CHUNK_SIZE);

        let size: u32 = match size.try_into() {
            Ok(size) => size,
            Err(e) => {
                // This shouldn't be possible: size should be a maximum of
                // PANTRY_MAX_CHUNK_SIZE due to the std::cmp::min above.
                error!(log, "size could not convert to u32: {e}");
                return;
            }
        };

        info!(
            log,
            "bulk read request: snapshot {} offset {} size {} using pantry {}",
            snapshot_id,
            offset,
            size,
            pantry_address,
        );

        let request =
            crucible_pantry_client::types::BulkReadRequest { offset, size };

        let response =
            match client.bulk_read(&volume_id.to_string(), &request).await {
                Ok(response) => response,

                Err(e) => {
                    error!(log, "{e}");
                    return;
                }
            };

        let crucible_pantry_client::types::BulkReadResponse {
            base64_encoded_data,
        } = response.into_inner();

        let bytes = match base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            base64_encoded_data,
        ) {
            Ok(bytes) => bytes,

            Err(e) => {
                error!(log, "error getting decoding base64 data: {e}");
                return;
            }
        };

        if let Err(e) = sender.send_data(bytes.into()).await {
            error!(log, "error sending decoded data: {e}");
            return;
        }

        offset += PANTRY_MAX_CHUNK_SIZE;
    }
}

impl super::Nexus {
    pub(crate) async fn user_data_export_for_snapshot(
        self: &Arc<Self>,
        opctx: &OpContext,
        snapshot_lookup: &lookup::Snapshot<'_>,
        maybe_range: Option<PotentialRange>,
    ) -> Result<Response<Body>, HttpError> {
        let (.., authz_snapshot, db_snapshot) =
            snapshot_lookup.fetch_for(authz::Action::Read).await?;

        let user_data_export = match self
            .db_datastore
            .user_data_export_lookup_for_snapshot(opctx, &authz_snapshot)
            .await?
        {
            Some(user_data_export) => user_data_export,

            None => {
                let s = format!(
                    "no user data export object for {}",
                    authz_snapshot.id()
                );
                warn!(self.log, "{s}");

                let s = String::from("snapshot not ready for export");
                let mut error = HttpError::for_internal_error(s);
                error.status_code = ErrorStatusCode::BAD_GATEWAY;
                return Err(error);
            }
        };

        let snapshot_size = db_snapshot.size.0.to_bytes();

        let maybe_range: Option<SingleRange> = match maybe_range {
            Some(range) => match range.parse(snapshot_size) {
                Ok(range) => Some(range),

                Err(body) => {
                    return Ok(body);
                }
            },

            None => None,
        };

        let pantry_address = user_data_export.pantry_address();

        // Check if it's gone from DNS, and fail early if so
        if self
            .is_internal_service_gone(
                ServiceName::CruciblePantry,
                pantry_address,
            )
            .await?
        {
            // XXX nuke user data export record here?

            let s = format!("pantry {pantry_address} is gone from DNS!");
            warn!(self.log, "{s}");

            let s = String::from("snapshot not ready for export");
            let mut error = HttpError::for_internal_error(s);
            error.status_code = ErrorStatusCode::BAD_GATEWAY;
            return Err(error);
        }

        let (sender, body) = Channel::<Bytes>::new(256); // XXX magic number

        tokio::spawn({
            let client = self.reqwest_client.clone();
            let log = self.log.new(o!("task" => "snapshot_blocks_read_task"));
            let snapshot_id = authz_snapshot.id();
            let volume_id = user_data_export.volume_id();
            let maybe_range = maybe_range.clone();

            async move {
                snapshot_blocks_read_task(
                    sender,
                    log,
                    client,
                    snapshot_id,
                    snapshot_size,
                    pantry_address,
                    maybe_range,
                    volume_id,
                )
                .await
            }
        });

        let mut builder = http::Response::builder()
            .header(http::header::CONTENT_TYPE, "application/octet-stream");

        // TODO Content-Disposition: attachment?

        if let Some(range) = maybe_range {
            builder = builder
                .status(http::StatusCode::PARTIAL_CONTENT)
                .header(http::header::CONTENT_RANGE, range.to_content_range())
                .header(http::header::ACCEPT_RANGES, "bytes")
                .header(
                    http::header::CONTENT_LENGTH,
                    range.content_length().get(),
                );
        } else {
            builder = builder
                .status(http::StatusCode::OK)
                .header(http::header::CONTENT_LENGTH, snapshot_size);
        }

        Ok(builder
            .body(dropshot::Body::wrap(body))
            .map_err(|e| HttpError::for_internal_error(e.to_string()))?)
    }
}
