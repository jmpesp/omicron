// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Simulated sled agent implementation

mod artifact_store;
mod collection;
mod config;
mod disk;
mod http_entrypoints;
mod http_entrypoints_pantry;
mod http_entrypoints_storage;
mod instance;
mod server;
mod simulatable;
mod sled_agent;
mod storage;
mod upstairs;

pub use crate::updates::ConfigUpdates;
pub use config::{
    Baseboard, Config, ConfigHardware, ConfigStorage, ConfigZpool, SimMode,
    TEST_HARDWARE_THREADS, TEST_RESERVOIR_RAM, ZpoolConfig,
};
pub use server::{RssArgs, Server, run_standalone_server};
pub use sled_agent::SledAgent;
pub use storage::PantryServer;
pub(crate) use storage::Storage;
pub use upstairs::SimulatedUpstairs;
