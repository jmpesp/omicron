// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use db_macros::Resource;
use nexus_db_schema::schema::silo_scim_client;
use uuid::Uuid;

/// A SCIM client sends requests to a SCIM provider (in this case, Nexus)
#[derive(Queryable, Insertable, Clone, Debug, Selectable, Resource)]
#[diesel(table_name = silo_scim_client)]
pub struct SiloScimClient {
    #[diesel(embed)]
    pub identity: SiloScimClientIdentity,

    pub silo_id: Uuid,

    /// A hard-coded token, expected for Bearer auth
    pub bearer_token: String,
}

