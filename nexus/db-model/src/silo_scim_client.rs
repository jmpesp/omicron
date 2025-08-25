// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use chrono::DateTime;
use chrono::Utc;
use nexus_db_schema::schema::silo_scim_client_bearer_token;
//use nexus_types::external_api::views;
use uuid::Uuid;

/// A SCIM client sends requests to a SCIM provider (in this case, Nexus) using
/// some sort of authentication. Nexus currently only supports Bearer token
/// auth from SCIM clients, and this is stored here.
// XXX rename file?
// XXX expiry
#[derive(Queryable, Insertable, Clone, Debug, Selectable)]
#[diesel(table_name = silo_scim_client_bearer_token)]
pub struct SiloScimClientBearerToken {
    pub id: Uuid,

    pub time_created: DateTime<Utc>,
    pub time_deleted: Option<DateTime<Utc>>,

    pub silo_id: Uuid,

    pub bearer_token: String,
}

/*
impl From<SiloScimClientBearerToken> for views::ScimBearerToken {
    fn from(t: SiloScimClientBearerToken) -> views::ScimBearerToken {
        views::ScimBearerToken {
            id: t.id,
            // XXX more fields?
            bearer_token: t.bearer_token,
        }
    }
}
*/
