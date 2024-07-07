CREATE TYPE IF NOT EXISTS omicron.public.snapshot_replacement_state AS ENUM (
  'requested',
  'allocating',
  'running',
  'replacement_done',
  'completing',
  'complete'
);
