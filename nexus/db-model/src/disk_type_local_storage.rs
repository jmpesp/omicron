// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::typed_uuid::DbTypedUuid;
use nexus_db_schema::schema::disk_type_local_storage;
use omicron_uuid_kinds::DatasetKind;
use omicron_uuid_kinds::DatasetUuid;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A Disk can be backed using a zvol slice from the local storage dataset
/// present on each zpool of a sled.
#[derive(
    Queryable, Insertable, Clone, Debug, Selectable, Serialize, Deserialize,
)]
#[diesel(table_name = disk_type_local_storage)]
pub struct DiskTypeLocalStorage {
    disk_id: Uuid,

    local_storage_dataset_allocation_id: Option<DbTypedUuid<DatasetKind>>,
}

impl DiskTypeLocalStorage {
    pub fn new(disk_id: Uuid) -> DiskTypeLocalStorage {
        DiskTypeLocalStorage {
            disk_id,
            local_storage_dataset_allocation_id: None,
        }
    }

    pub fn local_storage_dataset_allocation_id(&self) -> Option<DatasetUuid> {
        self.local_storage_dataset_allocation_id.map(Into::into)
    }
}
