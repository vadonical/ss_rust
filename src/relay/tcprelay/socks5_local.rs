//! Local server that accepts SOCKS 5 protocol

use std::{io, net::SocketAddr, sync::Arc};

use futures::future::{self, Either};
use log::{debug, error, info, trace, warn};
use tokio::{
    self,
    net::{
        tcp::{ReadHalf, WriteHalf},
        TcpListener,
        TcpStream,
    },
    prelude::*,
};

use crate::{
    config::ServerConfig,
    context::{Context, SharedContext},
};

use crate::relay::{
    loadbalancing::server::{LoadBalancer, PingBalancer},
    socks5::{self, Address, HandshakeRequest, HandshakeResponse, TcpRequestHeader, TcpResponseHeader},
};

use super::ignore_until_end;

#[derive(Debug, Clone)]
struct UdpConfig {
    enable_udp: bool,
    client_addr: SocketAddr,
}

async fn handle_socks5_connect<'a>(
    context: &Context,
    (mut r, mut w): (ReadHalf<'a>, WriteHalf<'a>),
    client_addr: SocketAddr,
    addr: &Address,
    svr_cfg: Arc<ServerConfig>,
) -> io::Result<()> {
    let svr_s = match super::connect_proxy_server(context, &*svr_cfg).await {
        Ok(svr_s) => {
            trace!("Proxy server connected, {:?}", svr_cfg);

            // Tell the client that we are ready
            let header = TcpResponseHeader::new(socks5::Reply::Succeeded, Address::SocketAddress(svr_s.local_addr()?));
            header.write_to(&mut w).await?;
            w.flush().await?;

            trace!("Sent header: {:?}", header);

            svr_s
        }
        Err(err) => {
            use crate::relay::socks5::Reply;
            use std::io::ErrorKind;

            error!("Failed to connect remote server {}, err: {}", svr_cfg.addr(), err);

            let reply = match err.kind() {
                ErrorKind::ConnectionRefused => Reply::ConnectionRefused,
                ErrorKind::ConnectionAborted => Reply::HostUnreachable,
                _ => Reply::NetworkUnreachable,
            };

            let header = TcpResponseHeader::new(
                reply,
                Address::SocketAddress("0.0.0.0:0".parse::<SocketAddr>().unwrap()),
            );
            header.write_to(&mut w).await?;
            w.flush().await?;

            return Err(err);
        }
    };

    let mut svr_s = super::proxy_server_handshake(svr_s, svr_cfg.clone(), addr).await?;
    let (mut svr_r, mut svr_w) = svr_s.split();

    use tokio::io::copy;

    let rhalf = copy(&mut r, &mut svr_w);
    let whalf = copy(&mut svr_r, &mut w);

    debug!(
        "CONNECT relay established {} <-> {} ({})",
        client_addr,
        svr_cfg.addr(),
        addr
    );

    match future::select(rhalf, whalf).await {
        Either::Left((Ok(..), _)) => trace!("CONNECT relay {} -> {} ({}) closed", client_addr, svr_cfg.addr(), addr),
        Either::Left((Err(err), _)) => trace!(
            "CONNECT relay {} -> {} ({}) closed with error {:?}",
            client_addr,
            svr_cfg.addr(),
            err,
            addr,
        ),
        Either::Right((Ok(..), _)) => trace!("CONNECT relay {} <- {} ({}) closed", client_addr, svr_cfg.addr(), addr),
        Either::Right((Err(err), _)) => trace!(
            "CONNECT relay {} <- {} ({}) closed with error {:?}",
            client_addr,
            svr_cfg.addr(),
            err,
            addr,
        ),
    }

    debug!(
        "CONNECT relay {} <-> {} ({}) closing",
        client_addr,
        svr_cfg.addr(),
        addr
    );

    Ok(())
}

#[allow(clippy::cognitive_complexity)]
async fn handle_socks5_client(
    context: &Context,
    mut s: TcpStream,
    conf: Arc<ServerConfig>,
    udp_conf: UdpConfig,
) -> io::Result<()> {
    if let Err(err) = s.set_keepalive(conf.timeout()) {
        error!("Failed to set keep alive: {:?}", err);
    }

    if context.config().no_delay {
        if let Err(err) = s.set_nodelay(true) {
            error!("Failed to set no delay: {:?}", err);
        }
    }

    let client_addr = s.peer_addr()?;

    let (mut r, mut w) = s.split();

    let handshake_req = HandshakeRequest::read_from(&mut r).await?;

    // Socks5 handshakes
    trace!("Socks5 {:?}", handshake_req);

    let (handshake_resp, res) = if !handshake_req.methods.contains(&socks5::SOCKS5_AUTH_METHOD_NONE) {
        let resp = HandshakeResponse::new(socks5::SOCKS5_AUTH_METHOD_NOT_ACCEPTABLE);
        warn!("Currently shadowsocks-rust does not support authentication");
        (
            resp,
            Err(io::Error::new(
                io::ErrorKind::Other,
                "Currently shadowsocks-rust does not support authentication",
            )),
        )
    } else {
        // Reply to client
        let resp = HandshakeResponse::new(socks5::SOCKS5_AUTH_METHOD_NONE);
        trace!("Reply handshake {:?}", resp);
        (resp, Ok(()))
    };

    handshake_resp.write_to(&mut w).await?;
    w.flush().await?;

    res?;

    // Fetch headers
    let header = match TcpRequestHeader::read_from(&mut r).await {
        Ok(h) => h,
        Err(err) => {
            error!("Failed to get TcpRequestHeader: {}", err);
            let rh = TcpResponseHeader::new(err.reply, Address::SocketAddress(client_addr));
            rh.write_to(&mut w).await?;
            return Err(From::from(err));
        }
    };

    trace!("Socks5 {:?}", header);

    let addr = header.address;
    match header.command {
        socks5::Command::TcpConnect => {
            let enable_tcp = context.config().mode.enable_tcp();
            if enable_tcp {
                debug!("CONNECT {}", addr);

                match handle_socks5_connect(context, (r, w), client_addr, &addr, conf).await {
                    Ok(..) => Ok(()),
                    Err(err) => Err(io::Error::new(
                        err.kind(),
                        format!("CONNECT {} failed with error \"{}\"", addr, err),
                    )),
                }
            } else {
                warn!("CONNECT is not enabled");
                let rh = TcpResponseHeader::new(socks5::Reply::CommandNotSupported, addr);
                rh.write_to(&mut w).await?;

                Ok(())
            }
        }
        socks5::Command::TcpBind => {
            warn!("BIND is not supported");
            let rh = TcpResponseHeader::new(socks5::Reply::CommandNotSupported, addr);
            rh.write_to(&mut w).await?;

            Ok(())
        }
        socks5::Command::UdpAssociate => {
            if udp_conf.enable_udp {
                debug!("UDP ASSOCIATE {}", addr);
                let rh = TcpResponseHeader::new(socks5::Reply::Succeeded, From::from(udp_conf.client_addr));
                rh.write_to(&mut w).await?;
                w.flush().await?;

                // Hold the connection until it ends by its own
                ignore_until_end(&mut r).await?;

                Ok(())
            } else {
                warn!("UDP ASSOCIATE is not enabled");
                let rh = TcpResponseHeader::new(socks5::Reply::CommandNotSupported, addr);
                rh.write_to(&mut w).await?;

                Ok(())
            }
        }
    }
}

/// Starts a TCP local server with Socks5 proxy protocol
pub async fn run(context: SharedContext) -> io::Result<()> {
    let local_addr = *context.config().local.as_ref().expect("Missing local config");

    let mut listener = TcpListener::bind(&local_addr)
        .await
        .unwrap_or_else(|err| panic!("Failed to listen on {}, {}", local_addr, err));

    let actual_local_addr = listener.local_addr().expect("Could not determine port bound to");

    info!("ShadowSocks TCP Listening on {}", actual_local_addr);

    let udp_conf = UdpConfig {
        enable_udp: context.config().mode.enable_udp(),
        client_addr: actual_local_addr,
    };

    let mut servers = PingBalancer::new(context.clone());

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let server_cfg = servers.pick_server();

        trace!("Got connection, addr: {}", peer_addr);
        trace!("Picked proxy server: {:?}", server_cfg);

        let context = context.clone();
        let udp_conf = udp_conf.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_socks5_client(&*context, socket, server_cfg, udp_conf).await {
                error!("Socks5 client {}", err);
            }
        });
    }
}
