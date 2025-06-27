// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! User data export objects

use super::sagas::user_data_export_delete;
use bytes::Bytes;
use dropshot::Body;
use dropshot::ErrorStatusCode;
use dropshot::HttpError;
use http::Response;
use http_body_util::channel::Channel;
use internal_dns_types::names::ServiceName;
use nexus_db_lookup::lookup;
use nexus_db_model::UserDataExport;
use nexus_db_model::UserDataExportRecord;
use nexus_db_model::UserDataExportResource;
use nexus_db_queries::authn;
use nexus_db_queries::authz;
use nexus_db_queries::context::OpContext;
use nexus_types::identity::Resource;
use omicron_common::api::external::Error;
use omicron_common::api::external::LookupResult;
use omicron_uuid_kinds::UserDataExportUuid;
use omicron_uuid_kinds::VolumeUuid;
use range_requests::PotentialRange;
use range_requests::SingleRange;
use slog::Logger;
use std::net::SocketAddrV6;
use std::sync::Arc;

#[allow(clippy::too_many_arguments)]
async fn user_data_export_blocks_read_task(
    mut sender: http_body_util::channel::Sender<Bytes>,
    log: Logger,
    client: reqwest::Client,
    resource: UserDataExportResource,
    total_size: u64,
    pantry_address: SocketAddrV6,
    maybe_range: Option<SingleRange>,
    volume_id: VolumeUuid,
) {
    let range_start: u64;
    let range_end: u64;

    (range_start, range_end) = match maybe_range {
        Some(range) => (range.start(), range.end_inclusive() + 1),
        None => (0, total_size),
    };

    info!(
        log,
        "read of resource {:?} start {} end {} using pantry {}",
        resource,
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

    while offset < range_end {
        let end = std::cmp::min(offset + PANTRY_MAX_CHUNK_SIZE, range_end);
        let size: u64 = end - offset;

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

        debug!(
            log,
            "read request: resource {:?} offset {} size {} using pantry {}",
            resource,
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
    async fn user_data_export_for_resource(
        &self,
        user_data_export: UserDataExportRecord,
        total_size: u64,
        maybe_range: Option<PotentialRange>,
    ) -> Result<Response<Body>, HttpError> {
        let maybe_range: Option<SingleRange> = match maybe_range {
            Some(range) => match range.parse(total_size) {
                Ok(range) => Some(range),

                Err(body) => {
                    return Ok(body);
                }
            },

            None => None,
        };

        let (pantry_address, volume_id) = match user_data_export.is_live() {
            Err(s) => {
                warn!(
                    self.log,
                    "error from is_live: {s}";
                    "user_data_export" => %user_data_export.id(),
                );

                let s = String::from("resource not ready for export");
                let mut error = HttpError::for_internal_error(s);
                error.status_code = ErrorStatusCode::BAD_GATEWAY;
                return Err(error);
            }

            Ok(UserDataExport::NotLive) => {
                // Record is not in state Live, and therefore will not work.
                let s = String::from("resource not ready for export");
                let mut error = HttpError::for_internal_error(s);
                error.status_code = ErrorStatusCode::BAD_GATEWAY;
                return Err(error);
            }

            Ok(UserDataExport::Live { pantry_address, volume_id }) => {
                (pantry_address, volume_id)
            }
        };

        // Check if the export's Pantry gone from DNS, and fail early if so. The
        // coordinator background task will eventually notice this too, delete
        // affected records, and recreate them on in-service Pantries.
        if self
            .is_internal_service_gone(
                ServiceName::CruciblePantry,
                pantry_address,
            )
            .await?
        {
            warn!(
                self.log,
                "pantry {pantry_address} is gone from DNS!";
                "user_data_export" => %user_data_export.id(),
            );

            let s = String::from("resource not ready for export");
            let mut error = HttpError::for_internal_error(s);
            error.status_code = ErrorStatusCode::BAD_GATEWAY;
            return Err(error);
        }

        // If a Pantry is restarted, it will lose all state, which includes any
        // volume attachments. Unless the coordinator background task checks
        // each attachment to see that it's still live, it will not notice this
        // (it currently does not!). Check to see that the user data export
        // volume is still attached.

        {
            let client = crucible_pantry_client::Client::new_with_client(
                &format!("http://{}", pantry_address),
                self.reqwest_client.clone(),
            );

            match client.volume_status(&volume_id.to_string()).await {
                Err(e) => {
                    warn!(
                        self.log,
                        "query pantry {pantry_address} for volume {volume_id} \
                        returned {e}";
                        "user_data_export" => %user_data_export.id(),
                    );

                    let delete_record = match e {
                        progenitor_client::Error::ErrorResponse(_) => {
                            // If the volume query returns a 404, then
                            // delete the user data export record, and allow
                            // another one to be created. This assumes that
                            // the Pantry bounced and the attachment was
                            // lost, and will cause the background task to
                            // create a new attachment.
                            //
                            // The Pantry could also return a 500, meaning
                            // it couldn't query the Volume for something.
                            // Delete the user data export record in this
                            // case as well.
                            true
                        }

                        progenitor_client::Error::CommunicationError(_) => {
                            // Expect communication errors to be transient,
                            // but this almost certainly means that Nexus
                            // won't be able to call the bulk_export
                            // endpoint, so also return a bad gateway error.
                            false
                        }

                        progenitor_client::Error::InvalidRequest(_)
                        | progenitor_client::Error::InvalidUpgrade(_)
                        | progenitor_client::Error::ResponseBodyError(_)
                        | progenitor_client::Error::InvalidResponsePayload(
                            _,
                            _,
                        )
                        | progenitor_client::Error::UnexpectedResponse(_)
                        | progenitor_client::Error::PreHookError(_)
                        | progenitor_client::Error::PostHookError(_) => {
                            // treat any other error as requiring a deletion
                            // plus recreating the attachment somewhere
                            // else.
                            true
                        }
                    };

                    if delete_record {
                        warn!(
                            self.log,
                            "based on error, marking record as deleted";
                            "user_data_export" => %user_data_export.id(),
                        );

                        self.db_datastore
                            .user_data_export_mark_deleted(
                                user_data_export.id(),
                            )
                            .await?;
                    }

                    let s = String::from("resource not ready for export");
                    let mut error = HttpError::for_internal_error(s);
                    error.status_code = ErrorStatusCode::BAD_GATEWAY;
                    return Err(error);
                }

                Ok(status) => {
                    let status = status.into_inner();

                    if !status.active {
                        // Hopefully this is a transient problem and the volume
                        // eventually activates! There's no way the bulk_read
                        // endpoint will work if the volume's not active, so
                        // bail here.

                        warn!(
                            self.log,
                            "pantry {pantry_address} for volume {volume_id} \
                            returned non-active!";
                            "user_data_export" => %user_data_export.id(),
                        );

                        let s = String::from("resource not ready for export");
                        let mut error = HttpError::for_internal_error(s);
                        error.status_code = ErrorStatusCode::BAD_GATEWAY;
                        return Err(error);
                    }

                    // Otherwise, proceed with the export
                }
            }
        }

        // Buffer 256 messages in memory, aka 128M of exported data.
        let (sender, body) = Channel::<Bytes>::new(256);

        tokio::spawn({
            let client = self.reqwest_client.clone();
            let log =
                self.log.new(o!("task" => "user_data_export_blocks_read_task"));
            let resource = user_data_export.resource();
            let maybe_range = maybe_range.clone();

            async move {
                user_data_export_blocks_read_task(
                    sender,
                    log,
                    client,
                    resource,
                    total_size,
                    pantry_address,
                    maybe_range,
                    volume_id,
                )
                .await
            }
        });

        let mut builder = http::Response::builder()
            .header(http::header::CONTENT_TYPE, "application/octet-stream")
            .header(http::header::CONTENT_DISPOSITION, "attachment");

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
                .header(http::header::CONTENT_LENGTH, total_size);
        }

        builder
            .body(dropshot::Body::wrap(body))
            .map_err(|e| HttpError::for_internal_error(e.to_string()))
    }

    pub(crate) async fn user_data_export_for_snapshot(
        self: &Arc<Self>,
        opctx: &OpContext,
        snapshot_lookup: &lookup::Snapshot<'_>,
        maybe_range: Option<PotentialRange>,
    ) -> Result<Response<Body>, HttpError> {
        let (.., db_snapshot) =
            snapshot_lookup.fetch_for(authz::Action::Read).await?;

        let snapshot_id = db_snapshot.id();

        let user_data_export = match self
            .db_datastore
            .user_data_export_lookup_for_snapshot(opctx, snapshot_id)
            .await?
        {
            Some(user_data_export) => user_data_export,

            None => {
                let s = format!(
                    "no user data export object for snapshot {snapshot_id}",
                );
                warn!(self.log, "{s}");

                let s = String::from("resource not ready for export");
                let mut error = HttpError::for_internal_error(s);
                error.status_code = ErrorStatusCode::BAD_GATEWAY;
                return Err(error);
            }
        };

        self.user_data_export_for_resource(
            user_data_export,
            db_snapshot.size.0.to_bytes(),
            maybe_range,
        )
        .await
    }

    pub(crate) async fn user_data_export_for_image(
        self: &Arc<Self>,
        opctx: &OpContext,
        image_lookup: &lookup::ImageLookup<'_>,
        maybe_range: Option<PotentialRange>,
    ) -> Result<Response<Body>, HttpError> {
        let (image_id, size) = match image_lookup {
            lookup::ImageLookup::ProjectImage(image) => {
                let (.., db_image) = image.fetch().await?;
                (db_image.id(), db_image.size)
            }

            lookup::ImageLookup::SiloImage(image) => {
                let (.., db_image) = image.fetch().await?;
                (db_image.id(), db_image.size)
            }
        };

        let maybe_user_data_export = self
            .db_datastore
            .user_data_export_lookup_for_image(opctx, image_id)
            .await?;

        let user_data_export = match maybe_user_data_export {
            Some(user_data_export) => user_data_export,

            None => {
                let s =
                    format!("no user data export object for image {image_id}",);
                warn!(self.log, "{s}");

                let s = String::from("resource not ready for export");
                let mut error = HttpError::for_internal_error(s);
                error.status_code = ErrorStatusCode::BAD_GATEWAY;
                return Err(error);
            }
        };

        self.user_data_export_for_resource(
            user_data_export,
            size.0.to_bytes(),
            maybe_range,
        )
        .await
    }

    pub async fn user_data_export_delete_by_id(
        &self,
        opctx: &OpContext,
        user_data_export_id: UserDataExportUuid,
    ) -> LookupResult<()> {
        let user_data_export = match self
            .datastore()
            .user_data_export_lookup_by_id(&opctx, user_data_export_id)
            .await?
        {
            Some(user_data_export) => user_data_export,
            None => {
                return Err(Error::non_resourcetype_not_found(format!(
                    "user data export {} not found",
                    user_data_export_id,
                )));
            }
        };

        let Some(volume_id) = user_data_export.volume_id() else {
            return Err(Error::internal_error(&format!(
                "user data export {} does not have a volume id!",
                user_data_export.id(),
            )));
        };

        let saga_params = user_data_export_delete::Params {
            serialized_authn: authn::saga::Serialized::for_opctx(opctx),
            user_data_export_id: user_data_export.id(),
            volume_id,
        };

        self.sagas
            .saga_execute::<user_data_export_delete::SagaUserDataExportDelete>(
                saga_params,
            )
            .await?;

        Ok(())
    }
}
