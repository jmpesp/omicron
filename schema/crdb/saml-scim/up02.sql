CREATE TABLE IF NOT EXISTS omicron.public.silo_scim_client_bearer_token (
    /* Identity metadata */
    id UUID PRIMARY KEY,

    time_created TIMESTAMPTZ NOT NULL,
    time_deleted TIMESTAMPTZ,

    silo_id UUID NOT NULL,

    bearer_token TEXT NOT NULL
);
