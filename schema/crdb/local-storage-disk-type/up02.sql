CREATE TABLE IF NOT EXISTS omicron.public.disk_type_local_storage (
    disk_id UUID PRIMARY KEY,

    local_storage_dataset_allocation_id UUID
);
