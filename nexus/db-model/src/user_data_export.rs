// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::typed_uuid::DbTypedUuid;
use crate::ipv6;
use nexus_db_schema::schema::user_data_export;
use omicron_uuid_kinds::VolumeKind;
use omicron_uuid_kinds::VolumeUuid;
use omicron_uuid_kinds::UserDataExportKind;
use omicron_uuid_kinds::UserDataExportUuid;
use std::net::SocketAddrV6;
use uuid::Uuid;
use crate::SqlU16;

/// XXX docstring
#[derive(Queryable, Insertable, Selectable, Clone, Debug)]
#[diesel(table_name = user_data_export)]
pub struct UserDataExport {
    id: DbTypedUuid<UserDataExportKind>,
    snapshot_id: Uuid,
    pantry_ip: ipv6::Ipv6Addr,
    pantry_port: SqlU16,
    volume_id: DbTypedUuid<VolumeKind>,
}

impl UserDataExport {
    pub fn new(
        id: UserDataExportUuid,
        snapshot_id: Uuid,
        pantry_address: SocketAddrV6,
        volume_id: VolumeUuid,
    ) -> Self {
        Self {
            id: id.into(),
            snapshot_id,
            pantry_ip: ipv6::Ipv6Addr::from(*pantry_address.ip()),
            pantry_port: SqlU16::from(pantry_address.port()),
            volume_id: volume_id.into(),
        }
    }

    pub fn id(&self) -> UserDataExportUuid {
        self.id.into()
    }

    pub fn snapshot_id(&self) -> Uuid {
        self.snapshot_id
    }

    pub fn pantry_address(&self) -> SocketAddrV6 {
        SocketAddrV6::new(self.pantry_ip.into(), *self.pantry_port, 0, 0)
    }

    pub fn volume_id(&self) -> VolumeUuid {
        self.volume_id.into()
    }
}
