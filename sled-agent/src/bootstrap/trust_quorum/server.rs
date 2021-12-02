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

/// A TCP server over which a secure SPDM channel will be established and an
/// application level trust protocol will run.
pub struct Server {
    log: Logger,
    share: Share,
    verifier: Verifier,
    listener: TcpListener,
}

impl Server {
    pub fn new(
        log: &Logger,
        share: Share,
        verifier: Verifier,
    ) -> io::Result<Self> {
        // TODO: Get port from config
        // TODO: Get IpAddr from local router:
        //   See https://github.com/oxidecomputer/omicron/issues/443
        let port: u16 = 7645;
        let addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);
        let sock = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        sock.set_only_v6(true)?;

        // Allow rebinding during linger
        sock.set_reuse_address(true)?;

        sock.bind(&addr.into())?;
        sock.listen(5)?;
        sock.set_nonblocking(true)?;

        Ok(Server {
            log: log.clone(),
            share,
            verifier,
            listener: TcpListener::from_std(sock.into())?,
        })
    }

    pub async fn run(&mut self) -> Result<(), BootstrapError> {
        loop {
            // TODO: Track the returned handles in a FuturesUnordered and log any errors?
            // Alternatively, maintain some shared state across all
            // responders that is accessable to the Server.
            let _ = self.accept().await?;
        }
    }

    async fn accept(
        &mut self,
    ) -> Result<JoinHandle<Result<(), BootstrapError>>, BootstrapError> {
        let (sock, addr) = self.listener.accept().await?;
        debug!(self.log, "Accepted connection from {}", addr);
        let share = self.share.clone();
        let verifier = self.verifier.clone();
        let log = self.log.clone();

        Ok(tokio::spawn(async move {
            run_responder(log, addr, sock, share, verifier).await
        }))
    }
}

async fn run_responder(
    log: Logger,
    addr: SocketAddr,
    sock: TcpStream,
    share: Share,
    verifier: Verifier,
) -> Result<(), BootstrapError> {
    let transport = spdm::Transport::new(sock);

    // TODO: Future code will return a secure SPDM session. For now, we just
    // return the framed transport so we can send unencrypted messages.
    let mut transport = spdm::responder::run(log.clone(), transport).await?;

    info!(log, "Sending share to {}", addr);

    let req = transport.recv(&log).await?;
    // There's only one possible request
    let _ = bincode::deserialize(&req)?;

    let rsp = Response::Share(share);
    let rsp = bincode::serialize(&rsp)?;
    transport.send(&rsp).await?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::super::rack_secret::RackSecret;
    use super::*;
    use assert_matches::assert_matches;

    #[tokio::test]
    async fn send_share() {
        // Create a rack secret and some shares
        let secret = RackSecret::new();
        let (shares, verifier) = secret.split(2, 2).unwrap();

        // Start a trust quorum server, but only accept one connection
        let log = omicron_test_utils::dev::test_slog_logger(
            "trust_quorum::send_share",
        );
        let mut server =
            Server::new(&log, shares[0].clone(), verifier).unwrap();
        let join_handle = tokio::spawn(async move { server.accept().await });

        // Connect a client to the trust quorum server and setup message framing
        let log2 = log.clone();
        let sock = TcpStream::connect("::1:7645").await.unwrap();
        let transport = spdm::Transport::new(sock);

        // Complete SPDM negotiation and return a "secure" transport.
        let mut transport = spdm::requester::run(log, transport).await.unwrap();

        // Request a share and receive it, validating it's what we expect.
        let req = bincode::serialize(&Request::Share).unwrap();
        transport.send(&req).await.unwrap();
        let rsp = transport.recv(&log2).await.unwrap();
        let rsp: Response = bincode::deserialize(&rsp).unwrap();
        assert_matches!(rsp, Response::Share(share) if share == shares[0]);

        join_handle.await.unwrap().unwrap();
    }
}
