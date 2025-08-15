// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::BlockSize;
use super::ByteCount;
use super::Generation;
use crate::typed_uuid::DbTypedUuid;
use crate::unsigned::SqlU8;
use chrono::DateTime;
use chrono::Utc;
use nexus_db_schema::schema::vmm_local_storage;
use nexus_db_schema::schema::instance_local_storage;
use nexus_db_schema::schema::local_storage_dataset;
use omicron_uuid_kinds::ExternalZpoolKind;
use omicron_uuid_kinds::ExternalZpoolUuid;
use omicron_uuid_kinds::DatasetUuid;
use std::convert::TryFrom;
use uuid::Uuid;
use db_macros::Asset;

/// A VMM local storage (passed through zvol)
#[derive(
    Queryable,
    Insertable,
    Clone,
    Debug,
    Selectable,
)]
#[diesel(table_name = vmm_local_storage)]
pub struct VmmLocalStorage { // XXX jwm rename to SledResourceVmmLocalStorage ?
    pub id: Uuid,

    // id for the project containing this Disk
    //pub project_id: Uuid,

    /// local storage is strongly tied to the vmm
    pub vmm_id: Uuid,

    /// The PCI slot (within the bank of slots reserved to disks) to which this
    /// disk should be attached if its attached instance is started, or None
    /// if there is no such assignment.
    ///
    /// Slot assignments are managed entirely in Nexus and aren't modified by
    /// runtime state changes in the sled agent, so this field is part of the
    /// "main" disk struct and not the runtime state (even though the attachment
    /// state and slot assignment will often change together).
    pub slot: SqlU8,

    /// size of the Disk
    #[diesel(column_name = size_bytes)]
    pub size: ByteCount,

    /// size of blocks (512, 2048, or 4096)
    pub block_size: BlockSize,

    pub pool_id: DbTypedUuid<ExternalZpoolKind>,
}

impl VmmLocalStorage {
    /*
    pub fn new(
        id: Uuid, // XXX typed
        //project_id: Uuid,
        vmm_id: Uuid,
        slot: u8,
        size: ByteCount,
        block_size: BlockSize,
        pool_id: ExternalZpoolUuid,
    ) -> Self {
        Self {
            id,
            //project_id,
            vmm_id,
            slot: slot.into(),
            size,
            block_size,
            pool_id: pool_id.into(),
        }
    }
    */

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn pool_id(&self) -> ExternalZpoolUuid {
        self.pool_id.into()
    }
}

/// XXX
#[derive(Queryable, Insertable, Debug, Clone, Selectable, Asset, PartialEq)]
#[diesel(table_name = local_storage_dataset)]
#[asset(uuid_kind = DatasetKind)]
pub struct LocalStorageDataset {
    #[diesel(embed)]
    identity: LocalStorageDatasetIdentity, // required?
    time_deleted: Option<DateTime<Utc>>, // required?
    rcgen: Generation,

    pub pool_id: Uuid,

    pub size_used: i64,
}

impl LocalStorageDataset {
    pub fn new(id: DatasetUuid, pool_id: Uuid) -> Self {
        Self {
            identity: LocalStorageDatasetIdentity::new(id),
            time_deleted: None,
            rcgen: Generation::new(),
            pool_id,
            size_used: 0,
        }
    }
}

/// XXX comment required
#[derive(
    Queryable,
    Insertable,
    Clone,
    Debug,
    Selectable,
)]
#[diesel(table_name = instance_local_storage)] // XXX table name
pub struct InstanceLocalStorageRequest {
    pub id: Uuid,

    // id for the project containing this Disk
    pub project_id: Uuid,

    pub instance_id: Uuid, // XXX typed

    /// The PCI slot (within the bank of slots reserved to disks) to which this
    /// disk should be attached if its attached instance is started, or None
    /// if there is no such assignment.
    ///
    /// Slot assignments are managed entirely in Nexus and aren't modified by
    /// runtime state changes in the sled agent, so this field is part of the
    /// "main" disk struct and not the runtime state (even though the attachment
    /// state and slot assignment will often change together).
    pub slot: SqlU8,

    /// size of the Disk
    #[diesel(column_name = size_bytes)]
    pub size: ByteCount,

    /// size of blocks (512, 2048, or 4096)
    pub block_size: BlockSize,
}

// XXX what deletes these?
