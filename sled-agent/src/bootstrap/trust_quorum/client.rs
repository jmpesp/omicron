// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

use slog::Logger;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use vsss_rs::Share;

use super::msgs::{Request, Response};
use super::rack_secret::Verifier;
use crate::bootstrap::{agent::BootstrapError, spdm};

pub struct Client {
    log: Logger,
    verifier: Verifier,
    addr: SocketAddr,
}

impl Client {
    pub fn new(log: &Logger, verifier: Verifier, addr: SocketAddr) -> Client {
        Client { log: log.clone(), verifier, addr }
    }

    // Connect to a trust quorum server, establish an SPDM channel, and retrieve
    // a share.
    pub async fn get_share(&self) -> Result<Share, BootstrapError> {
        let sock = TcpStream::connect(&self.addr).await?;
        let transport = spdm::Transport::new(sock);

        // Complete SPDM negotiation and return a secure transport
        let mut transport =
            spdm::requester::run(self.log.clone(), transport).await?;

        // Request a share and receive it, validating it's what we expect.
        let req = bincode::serialize(&Request::Share)?;
        transport.send(&req).await?;

        // TODO: Add a receive timeout
        let rsp = transport.recv(&self.log).await.unwrap();
        let rsp: Response = bincode::deserialize(&rsp)?;

        if let Response::Share(share) = rsp {
            if self.verifier.verify(&share) {
                Ok(share)
            } else {
                Err(BootstrapError::InvalidShare(self.addr.clone()))
            }
        } else {
            Err(BootstrapError::InvalidMsg(self.addr.clone()))
        }
    }
}
