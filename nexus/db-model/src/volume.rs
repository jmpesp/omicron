// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::{Generation, Region};
use crate::collection::DatastoreCollectionConfig;
use chrono::{DateTime, Utc};
use db_macros::Asset;
use nexus_db_schema::schema::{region, volume};
use omicron_uuid_kinds::VolumeUuid;
use uuid::Uuid;

#[derive(Asset, Queryable, Insertable, Debug, Selectable, Clone)]
#[diesel(table_name = volume)]
#[asset(uuid_kind = VolumeKind)]
pub struct Volume {
    #[diesel(embed)]
    identity: VolumeIdentity,
    pub time_deleted: Option<DateTime<Utc>>,

    rcgen: Generation,

    data: String,

    pub resources_to_clean_up: Option<String>,

    /// The version of the higher level Volume enum serialized into the data
    /// column
    version: Generation,

    /// Generation number for optimistic concurrency when updating the
    /// serialized Volume enum
    #[diesel(column_name = "gen")]
    generation: Generation,
}

impl Volume {
    pub fn new(id: VolumeUuid, data: String) -> Self {
        Self {
            identity: VolumeIdentity::new(id),
            time_deleted: None,
            rcgen: Generation::new(),
            data,
            resources_to_clean_up: None,
            version: Generation::new(),
            generation: Generation::new(),
        }
    }

    pub fn generation(&self) -> i64 {
        (&self.generation.0).into()
    }

    pub fn data(&self) -> &str {
        &self.data
    }
}

// Volumes contain regions
impl DatastoreCollectionConfig<Region> for Volume {
    type CollectionId = Uuid;
    type GenerationNumberColumn = volume::dsl::rcgen;
    type CollectionTimeDeletedColumn = volume::dsl::time_deleted;
    type CollectionIdColumn = region::dsl::volume_id;
}
