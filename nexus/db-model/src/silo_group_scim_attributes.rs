// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use nexus_db_schema::schema::silo_group_scim_attributes;
use uuid::Uuid;

/// SCIM groups have extra attributes sent by the provisioning client, and they
/// need to be persisted by the SCIM server.
#[derive(Queryable, Insertable, Debug, Selectable, Clone)]
#[diesel(table_name = silo_group_scim_attributes)]
pub struct SiloGroupScimAttributes {
    pub silo_group_id: Uuid,

    // SCIM attributes supported by scim2-rs

    pub display_name: String,
    pub external_id: Option<String>,
}
