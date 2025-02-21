use std::borrow::Cow;
use std::future::Future;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use axum::{Extension, Router};
use axum::handler::Handler;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::http::header::HeaderName;
use axum::response::IntoResponse;
use axum_client_ip::ClientIp;
use axum::routing::get;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use log::{debug, info};
use scc::ebr::Arc;
use bip_bencode::{ben_bytes, ben_int, ben_list, ben_map, BMutAccess};
use scc::HashIndex;
use crate::common::parse_query;
use crate::handlers::{handle_announce, handle_scrape, validate_announce, validate_scrape};
use crate::tracker::{StatsEvent, TorrentTracker};

pub async fn http_service(handle: Handle, addr: SocketAddr, data: Arc<TorrentTracker>) -> impl Future<Output = Result<(), std::io::Error>>
{
    info!("[HTTP] Starting server listener on {}", addr);
    axum_server::bind(addr)
        .handle(handle)
        .serve(Router::new()
            .route("/announce", get(http_service_announce))
            .route("/scrape", get(http_service_scrape))
            .fallback(http_service_404.into_service())
            .layer(Extension(data))
            .into_make_service_with_connect_info::<SocketAddr>()
        )
}

pub async fn https_service(handle: Handle, addr: SocketAddr, data: Arc<TorrentTracker>, ssl_key: String, ssl_cert: String) -> impl Future<Output = Result<(), std::io::Error>>
{
    let ssl_config = RustlsConfig::from_pem_file(
        ssl_cert.clone(),
        ssl_key.clone()
    ).await.unwrap();

    info!("[HTTPS] Starting server listener on {}", addr);
    axum_server::bind_rustls(addr, ssl_config)
        .handle(handle)
        .serve(Router::new()
            .route("/announce", get(http_service_announce))
            .route("/scrape", get(http_service_scrape))
            .fallback(http_service_404.into_service())
            .layer(Extension(data))
            .into_make_service_with_connect_info::<SocketAddr>()
        )
}

pub async fn http_service_announce(ClientIp(ip): ClientIp, axum::extract::RawQuery(params): axum::extract::RawQuery, Extension(state): Extension<Arc<TorrentTracker>>) -> (StatusCode, HeaderMap, Vec<u8>)
{
    http_service_announce_log(ip, state.clone()).await;
    let mut headers = HeaderMap::new();
    headers.insert(HeaderName::from_static("content-type"), HeaderValue::from_static("text/plain"));

    let query_map_result = parse_query(params);
    let query_map: HashIndex<String, Vec<Vec<u8>>> = match query_map_result {
        Ok(e) => {
            e
        }
        Err(e) => {
            let return_string = (ben_map! {"failure reason" => ben_bytes!(e.to_string())}).encode();
            return (StatusCode::OK, headers, return_string);
        }
    };

    let announce = validate_announce(state.clone().config.clone(), ip, query_map).await;
    let announce_unwrapped = match announce {
        Ok(result) => { result }
        Err(e) => {
            let return_string = (ben_map! {"failure reason" => ben_bytes!(e.to_string())}).encode();
            return (StatusCode::OK, headers, return_string);
        }
    };

    let (_torrent_peer, torrent_entry) = match handle_announce(state.clone(), announce_unwrapped.clone()).await {
        Ok(result) => { result }
        Err(e) => {
            let return_string = (ben_map! {"failure reason" => ben_bytes!(e.to_string())}).encode();
            return (StatusCode::OK, headers, return_string);
        }
    };

    if announce_unwrapped.clone().compact {
        let mut peers: Vec<u8> = Vec::new();
        for (_peer_id, torrent_peer) in torrent_entry.peers.iter() {
            let _ = match torrent_peer.peer_addr.ip() {
                IpAddr::V4(ip) => peers.write(&u32::from(ip).to_be_bytes()),
                IpAddr::V6(ip) => peers.write(&u128::from(ip).to_be_bytes())
            };
            peers.write_all(&announce_unwrapped.clone().port.to_be_bytes()).unwrap();
        }
        return if announce_unwrapped.clone().remote_addr.is_ipv4() {
            let return_string = (ben_map! {
                "interval" => ben_int!(state.config.interval.unwrap() as i64),
                "min interval" => ben_int!(state.config.interval_minimum.unwrap() as i64),
                "complete" => ben_int!(torrent_entry.seeders as i64),
                "incomplete" => ben_int!(torrent_entry.leechers as i64),
                "downloaded" => ben_int!(torrent_entry.completed as i64),
                "peers" => ben_bytes!(peers.clone())
            }).encode();
            (StatusCode::OK, headers, return_string)
        } else {
            let return_string = (ben_map! {
                "interval" => ben_int!(state.config.interval.unwrap() as i64),
                "min interval" => ben_int!(state.config.interval_minimum.unwrap() as i64),
                "complete" => ben_int!(torrent_entry.seeders as i64),
                "incomplete" => ben_int!(torrent_entry.leechers as i64),
                "downloaded" => ben_int!(torrent_entry.completed as i64),
                "peers6" => ben_bytes!(peers.clone())
            }).encode();
            (StatusCode::OK, headers, return_string)
        }
    }

    let mut peers_list = ben_list!();
    let peers_list_mut = peers_list.list_mut().unwrap();
    for (peer_id, torrent_peer) in torrent_entry.peers.iter() {
        let _ = match torrent_peer.peer_addr.ip() {
            IpAddr::V4(_) => {
                peers_list_mut.push(ben_map! {
                    "peer id" => ben_bytes!(peer_id.clone().to_string()),
                    "ip" => ben_bytes!(torrent_peer.peer_addr.ip().to_string()),
                    "port" => ben_int!(torrent_peer.peer_addr.port() as i64)
                });
            },
            IpAddr::V6(_) => {
                peers_list_mut.push(ben_map! {
                    "peer id" => ben_bytes!(peer_id.clone().to_string()),
                    "ip" => ben_bytes!(torrent_peer.peer_addr.ip().to_string()),
                    "port" => ben_int!(torrent_peer.peer_addr.port() as i64)
                });
            }
        };
    }
    if announce_unwrapped.clone().remote_addr.is_ipv4() {
        let return_string = (ben_map! {
            "interval" => ben_int!(state.config.interval.unwrap() as i64),
            "min interval" => ben_int!(state.config.interval_minimum.unwrap() as i64),
            "complete" => ben_int!(torrent_entry.seeders as i64),
            "incomplete" => ben_int!(torrent_entry.leechers as i64),
            "downloaded" => ben_int!(torrent_entry.completed as i64),
            "peers" => peers_list.clone()
        }).encode();
        (StatusCode::OK, headers, return_string)
    } else {
        let return_string = (ben_map! {
            "interval" => ben_int!(state.config.interval.unwrap() as i64),
            "min interval" => ben_int!(state.config.interval_minimum.unwrap() as i64),
            "complete" => ben_int!(torrent_entry.seeders as i64),
            "incomplete" => ben_int!(torrent_entry.leechers as i64),
            "downloaded" => ben_int!(torrent_entry.completed as i64),
            "peers6" => peers_list.clone()
        }).encode();
        (StatusCode::OK, headers, return_string)
    }
}

pub async fn http_service_announce_log(ip: IpAddr, tracker: Arc<TorrentTracker>)
{
    if ip.is_ipv4() {
        debug!("[HTTP REQUEST] TCPv4 Announcement received from {}", ip.to_string());
        tracker.clone().update_stats(StatsEvent::Tcp4ConnectionsHandled, 1).await;
        tracker.clone().update_stats(StatsEvent::Tcp4AnnouncesHandled, 1).await;
    } else {
        debug!("[HTTP REQUEST] TCPv6 Announcement received from {}", ip.to_string());
        tracker.clone().update_stats(StatsEvent::Tcp6ConnectionsHandled, 1).await;
        tracker.clone().update_stats(StatsEvent::Tcp6AnnouncesHandled, 1).await;
    }
}

pub async fn http_service_scrape(ClientIp(ip): ClientIp, axum::extract::RawQuery(params): axum::extract::RawQuery, Extension(state): Extension<Arc<TorrentTracker>>) -> (StatusCode, HeaderMap, Vec<u8>)
{
    http_service_scrape_log(ip, state.clone()).await;
    let mut headers = HeaderMap::new();
    headers.insert(HeaderName::from_static("content-type"), HeaderValue::from_static("text/plain"));

    let query_map_result = parse_query(params);
    let query_map: HashIndex<String, Vec<Vec<u8>>> = match query_map_result {
        Ok(e) => {
            e
        }
        Err(e) => {
            let return_string = (ben_map! {"failure reason" => ben_bytes!(e.to_string())}).encode();
            return (StatusCode::OK, headers, return_string);
        }
    };

    let scrape = validate_scrape(state.clone().config.clone(), ip, query_map).await;
    return match scrape {
        Ok(e) => {
            let data_scrape = handle_scrape(state.clone(), e.clone()).await;
            let mut scrape_list = ben_map!();
            let scrape_list_mut = scrape_list.dict_mut().unwrap();
            for (key, value) in data_scrape.iter() {
                scrape_list_mut.insert(Cow::from(key.0.to_vec()), ben_map! {
                    "complete" => ben_int!(value.seeders as i64),
                    "downloaded" => ben_int!(value.completed as i64),
                    "incomplete" => ben_int!(value.leechers as i64)
                });
            }
            let return_string = (ben_map! {
                "interval" => ben_int!(state.config.interval.unwrap() as i64),
                "min interval" => ben_int!(state.config.interval_minimum.unwrap() as i64),
                "files" => scrape_list
            }).encode();
            (StatusCode::OK, headers, return_string)
        }
        Err(e) => {
            let return_string = (ben_map! {"failure reason" => ben_bytes!(e.to_string())}).encode();
            (StatusCode::OK, headers, return_string)
        }
    };
}

pub async fn http_service_scrape_log(ip: IpAddr, tracker: Arc<TorrentTracker>)
{
    if ip.is_ipv4() {
        debug!("[HTTP REQUEST] TCPv4 Scrape received from {}", ip.to_string());
        tracker.clone().update_stats(StatsEvent::Tcp4ConnectionsHandled, 1).await;
        tracker.clone().update_stats(StatsEvent::Tcp4ScrapesHandled, 1).await;
    } else {
        debug!("[HTTP REQUEST] TCPv6 Scrape received from {}", ip.to_string());
        tracker.clone().update_stats(StatsEvent::Tcp6ConnectionsHandled, 1).await;
        tracker.clone().update_stats(StatsEvent::Tcp6ScrapesHandled, 1).await;
    }
}

pub async fn http_service_404(ClientIp(ip): ClientIp, axum::extract::RawQuery(_params): axum::extract::RawQuery, Extension(state): Extension<Arc<TorrentTracker>>) -> impl IntoResponse
{
    http_service_404_log(ip, state.clone()).await;
    let return_string = (ben_map! {"failure reason" => ben_bytes!("unknown request")}).encode();
    let body = std::str::from_utf8(&return_string).unwrap().to_string();
    (StatusCode::NOT_FOUND, body)
}

pub async fn http_service_404_log(ip: IpAddr, tracker: Arc<TorrentTracker>)
{
    if ip.is_ipv4() {
        tracker.clone().update_stats(StatsEvent::Tcp4ConnectionsHandled, 1).await;
    } else {
        tracker.clone().update_stats(StatsEvent::Tcp6ConnectionsHandled, 1).await;
    }
}

