use std::io::Cursor;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::exit;
use log::{error, info, debug};
use scc::ebr::Arc;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use crate::udp_common;
use crate::common::{AnnounceEvent, AnnounceQueryRequest, InfoHash, PeerId};
use crate::handlers::handle_announce;
use crate::tracker::{StatsEvent, TorrentEntry, TorrentTracker};
use crate::udp_common::{AnnounceInterval, AnnounceRequest, AnnounceResponse, ConnectRequest, ConnectResponse, ErrorResponse, get_connection_id, NumberOfDownloads, NumberOfPeers, Port, Request, Response, ResponsePeer, ScrapeRequest, ScrapeResponse, ServerError, TorrentScrapeStatistics, TransactionId};

const MAX_SCRAPE_TORRENTS: u8 = 74;
const MAX_PACKET_SIZE: usize = 1496;

pub struct UdpServer {
    socket: Arc<UdpSocket>,
    tracker: Arc<TorrentTracker>
}

impl UdpServer {
    pub async fn new(tracker: Arc<TorrentTracker>, bind_address: SocketAddr) -> tokio::io::Result<UdpServer>
    {
        let socket = UdpSocket::bind(bind_address).await?;
        Ok(UdpServer {
            socket: Arc::new(socket),
            tracker,
        })
    }

    pub async fn start(&self, rx: tokio::sync::watch::Receiver<bool>)
    {
        loop {
            let mut rx = rx.clone();
            let mut data = [0; 65507];
            let socket = self.socket.clone();
            let tracker = self.tracker.clone();

            tokio::select! {
                _ = rx.changed() => {
                    info!("Stopping UDP server: {}...", socket.local_addr().unwrap());
                    break;
                }
                Ok((valid_bytes, remote_addr)) = socket.recv_from(&mut data) => {
                    let payload = data[..valid_bytes].to_vec();

                    debug!("Received {} bytes from {}", payload.len(), remote_addr);
                    debug!("{:?}", payload);

                    let response = handle_packet(remote_addr, payload, tracker).await;
                    UdpServer::send_response(socket, remote_addr, response).await;
                }
            }
        }
    }

    async fn send_response(socket: Arc<UdpSocket>, remote_addr: SocketAddr, response: Response) {
        debug!("sending response to: {:?}", &remote_addr);

        let buffer = vec![0u8; MAX_PACKET_SIZE];
        let mut cursor = Cursor::new(buffer);

        match response.write(&mut cursor) {
            Ok(_) => {
                let position = cursor.position() as usize;
                let inner = cursor.get_ref();

                debug!("{:?}", &inner[..position]);
                UdpServer::send_packet(socket, &remote_addr, &inner[..position]).await;
            }
            Err(_) => { debug!("could not write response to bytes."); }
        }
    }

    async fn send_packet(socket: Arc<UdpSocket>, remote_addr: &SocketAddr, payload: &[u8]) {
        // doesn't matter if it reaches or not
        let _ = socket.send_to(payload, remote_addr).await;
    }
}

pub async fn udp_service(addr: SocketAddr, data: Arc<TorrentTracker>, rx: tokio::sync::watch::Receiver<bool>) -> JoinHandle<()>
{
    let udp_server = UdpServer::new(data, addr).await.unwrap_or_else(|e| {
        error!("Could not listen to the UDP port: {}", e);
        exit(1);
    });

    info!("[UDP] Starting server listener on {}", addr);
    tokio::spawn(async move {
        udp_server.start(rx).await;
    })
}

pub async fn handle_packet(remote_addr: SocketAddr, payload: Vec<u8>, tracker: Arc<TorrentTracker>) -> Response {
    match Request::from_bytes(&payload[..payload.len()], MAX_SCRAPE_TORRENTS).map_err(|_| ServerError::InternalServerError) {
        Ok(request) => {
            let transaction_id = match &request {
                Request::Connect(connect_request) => {
                    connect_request.transaction_id
                }
                Request::Announce(announce_request) => {
                    announce_request.transaction_id
                }
                Request::Scrape(scrape_request) => {
                    scrape_request.transaction_id
                }
            };

            match handle_request(request, remote_addr, tracker).await {
                Ok(response) => response,
                Err(e) => handle_udp_error(e, transaction_id)
            }
        }
        // bad request
        Err(_) => handle_udp_error(ServerError::BadRequest, TransactionId(0))
    }
}

pub async fn handle_request(request: Request, remote_addr: SocketAddr, tracker: Arc<TorrentTracker>) -> Result<Response, ServerError> {
    match request {
        Request::Connect(connect_request) => {
            handle_udp_connect(remote_addr, &connect_request, tracker).await
        }
        Request::Announce(announce_request) => {
            handle_udp_announce(remote_addr, &announce_request, tracker).await
        }
        Request::Scrape(scrape_request) => {
            handle_udp_scrape(remote_addr, &scrape_request, tracker).await
        }
    }
}

pub async fn handle_udp_connect(remote_addr: SocketAddr, request: &ConnectRequest, tracker: Arc<TorrentTracker>) -> Result<Response, ServerError> {
    let connection_id = get_connection_id(&remote_addr);

    let response = Response::from(ConnectResponse {
        transaction_id: request.transaction_id,
        connection_id,
    });

    // send stats event
    match remote_addr {
        SocketAddr::V4(_) => { tracker.update_stats(StatsEvent::Udp4ConnectionsHandled, 1).await; }
        SocketAddr::V6(_) => { tracker.update_stats(StatsEvent::Udp6ConnectionsHandled, 1).await; }
    };

    Ok(response)
}

pub async fn handle_udp_announce(remote_addr: SocketAddr, request: &AnnounceRequest, tracker: Arc<TorrentTracker>) -> Result<Response, ServerError> {
    // let peer = TorrentPeer::from_udp_announce_request(&request.clone(), remote_addr.ip());
    let event = match request.event {
        udp_common::AnnounceEvent::Started => { AnnounceEvent::Started }
        udp_common::AnnounceEvent::Stopped => { AnnounceEvent::Stopped }
        udp_common::AnnounceEvent::Completed => { AnnounceEvent::Completed }
        udp_common::AnnounceEvent::None => { AnnounceEvent::None }
    };

    let _ = match tracker.get_torrent(InfoHash(request.info_hash.0)).await {
        None => {
            if tracker.config.persistency {
                tracker.add_torrent(InfoHash(request.info_hash.0), TorrentEntry::new(), true).await;
            } else {
                tracker.add_torrent(InfoHash(request.info_hash.0), TorrentEntry::new(), false).await;
            }
            TorrentEntry::new()
        }
        Some(result) => { result }
    };

    // Handle the request data.
    match handle_announce(tracker.clone(), AnnounceQueryRequest {
        info_hash: InfoHash(request.info_hash.0),
        peer_id: PeerId(request.peer_id.0),
        port: request.port.0,
        uploaded: request.bytes_uploaded.0 as u64,
        downloaded: request.bytes_downloaded.0 as u64,
        left: request.bytes_left.0 as u64,
        compact: false,
        no_peer_id: false,
        event,
        remote_addr: remote_addr.ip(),
        numwant: request.peers_wanted.0 as u64
    }).await {
        Ok(result) => { result }
        Err(_) => {
            return Err(ServerError::InternalServerError);
        }
    };

    // get all peers excluding the client_addr
    let peers = tracker.get_torrent(InfoHash(request.info_hash.0)).await;
    if peers.is_none() {
        return Err(ServerError::UnknownInfoHash);
    }

    // Build the response data.
    let announce_response = if remote_addr.is_ipv4() {
        Response::from(AnnounceResponse {
            transaction_id: request.transaction_id,
            announce_interval: AnnounceInterval(tracker.config.interval.unwrap() as i32),
            leechers: NumberOfPeers(peers.clone().unwrap().leechers as i32),
            seeders: NumberOfPeers(peers.clone().unwrap().seeders as i32),
            peers: peers.clone().unwrap().peers.iter()
                .filter_map(|(_peer_id, torrent_entry)| if torrent_entry.peer_addr.is_ipv4() {
                    Some(ResponsePeer::<Ipv4Addr> {
                        ip_address: torrent_entry.peer_addr.ip().to_string().parse::<Ipv4Addr>().unwrap(),
                        port: Port(torrent_entry.peer_addr.port())
                    })
                } else {
                    None
                }).collect()
        })
    } else {
        Response::from(AnnounceResponse {
            transaction_id: request.transaction_id,
            announce_interval: AnnounceInterval(tracker.config.clone().interval.unwrap() as i32),
            leechers: NumberOfPeers(peers.clone().unwrap().leechers as i32),
            seeders: NumberOfPeers(peers.clone().unwrap().seeders as i32),
            peers: peers.clone().unwrap().peers.iter()
                .filter_map(|(_peer_id, torrent_entry)| if torrent_entry.peer_addr.is_ipv6() {
                    Some(ResponsePeer::<Ipv6Addr> {
                        ip_address: torrent_entry.peer_addr.ip().to_string().parse::<Ipv6Addr>().unwrap(),
                        port: Port(torrent_entry.peer_addr.port())
                    })
                } else {
                    None
                }).collect()
        })
    };

    // send stats event
    if remote_addr.is_ipv4() {
        tracker.update_stats(StatsEvent::Udp4AnnouncesHandled, 1).await;
    } else {
        tracker.update_stats(StatsEvent::Udp6AnnouncesHandled, 1).await;
    }

    Ok(announce_response)
}

pub async fn handle_udp_scrape(remote_addr: SocketAddr, request: &ScrapeRequest, tracker: Arc<TorrentTracker>) -> Result<Response, ServerError> {
    let mut torrent_stats: Vec<TorrentScrapeStatistics> = Vec::new();
    for info_hash in request.info_hashes.iter() {
        let info_hash = InfoHash(info_hash.0);
        let scrape_entry = match tracker.get_torrent(InfoHash(info_hash.0)).await {
            None => {
                TorrentScrapeStatistics {
                    seeders: NumberOfPeers(0),
                    completed: NumberOfDownloads(0),
                    leechers: NumberOfPeers(0)
                }
            }
            Some(torrent_info) => {
                TorrentScrapeStatistics {
                    seeders: NumberOfPeers(torrent_info.seeders as i32),
                    completed: NumberOfDownloads(torrent_info.completed as i32),
                    leechers: NumberOfPeers(torrent_info.leechers as i32),
                }
            }
        };
        torrent_stats.push(scrape_entry);
    }

    // send stats event
    if remote_addr.is_ipv4() {
        tracker.update_stats(StatsEvent::Udp4ScrapesHandled, 1).await;
    } else {
        tracker.update_stats(StatsEvent::Udp6ScrapesHandled, 1).await;
    }

    return Ok(Response::from(ScrapeResponse {
        transaction_id: request.transaction_id,
        torrent_stats
    }));
}

fn handle_udp_error(e: ServerError, transaction_id: TransactionId) -> Response {
    let message = e.to_string();
    Response::from(ErrorResponse { transaction_id, message: message.into() })
}
