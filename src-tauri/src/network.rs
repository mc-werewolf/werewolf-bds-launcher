use futures_util::{SinkExt, StreamExt};
use igd_next::{aio::tokio::search_gateway, PortMappingProtocol, SearchOptions};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::UdpSocket as TokioUdpSocket, sync::mpsc};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};

const BDS_PORT: u16 = 19132;
const DIRECTORY_URL: &str = "https://mc-werewolf.com/api/network/v1/servers";

#[derive(Clone, Default)]
pub struct NetworkState(Arc<Mutex<Option<Session>>>);

#[derive(Clone)]
struct Session {
    id: String,
    token: String,
    endpoint: Option<Endpoint>,
}

#[derive(Clone)]
struct Endpoint {
    host_name: String,
    host_port: u16,
    mode: &'static str,
}

#[derive(Deserialize)]
struct RegistrationResponse {
    id: String,
    token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub server_id: String,
    pub public_address: Option<String>,
    pub local_address: Option<String>,
    pub port: u16,
    pub firewall_requested: bool,
    pub upnp_mapped: bool,
    pub warning: Option<String>,
}

pub async fn publish(state: NetworkState) -> Result<PublishResult, String> {
    let firewall_requested = request_firewall_rule()?;
    let direct = discover_direct_endpoint().await;
    let (mut endpoint, local_address, upnp_mapped, mut warning) = match direct {
        Ok((endpoint, local_address)) if is_public_ip(endpoint.host_name.parse().unwrap()) => {
            (Some(endpoint), Some(local_address), true, None)
        }
        Ok((_endpoint, local_address)) => (
            None,
            Some(local_address),
            true,
            Some(
                "CGNATまたは二重ルーターを検出しました。中央中継の割当を待っています。".to_owned(),
            ),
        ),
        Err(error) => (
            None,
            None,
            false,
            Some(format!("{error} 中央中継の割当を待っています。")),
        ),
    };
    let session = register(endpoint.clone()).await?;
    heartbeat(&session).await?;
    *state
        .0
        .lock()
        .map_err(|_| "ネットワーク状態を保存できませんでした")? = Some(session.clone());
    spawn_heartbeat(state.clone());
    if endpoint.is_none() {
        spawn_relay(state.clone(), session.clone());
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let assigned = state
                .0
                .lock()
                .ok()
                .and_then(|value| value.as_ref().and_then(|value| value.endpoint.clone()));
            if assigned.is_some() {
                endpoint = assigned;
                warning = None;
                break;
            }
        }
    }
    Ok(PublishResult {
        server_id: session.id,
        public_address: endpoint
            .as_ref()
            .map(|value| format!("{}:{}", value.host_name, value.host_port)),
        local_address,
        port: BDS_PORT,
        firewall_requested,
        upnp_mapped,
        warning,
    })
}

async fn discover_direct_endpoint() -> Result<(Endpoint, String), String> {
    let gateway = search_gateway(SearchOptions {
        timeout: Some(Duration::from_secs(10)),
        single_search_timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    })
    .await
    .map_err(|error| format!("UPnP対応ルーターを検出できませんでした: {error}."))?;
    let local_ip = local_ip_for_gateway(gateway.addr)?;
    let local_address = SocketAddr::new(local_ip, BDS_PORT);
    gateway
        .add_port(
            PortMappingProtocol::UDP,
            BDS_PORT,
            local_address,
            0,
            "Werewolf Bedrock Dedicated Server",
        )
        .await
        .map_err(|error| format!("ルーターでUDP {BDS_PORT}を開放できませんでした: {error}"))?;
    let public_ip = gateway
        .get_external_ip()
        .await
        .map_err(|error| format!("ルーターの公開IPを取得できませんでした: {error}"))?;
    Ok((
        Endpoint {
            host_name: public_ip.to_string(),
            host_port: BDS_PORT,
            mode: "direct",
        },
        local_address.to_string(),
    ))
}

async fn register(endpoint: Option<Endpoint>) -> Result<Session, String> {
    let client = reqwest::Client::new();
    let registration = client
        .post(DIRECTORY_URL)
        .json(&serde_json::json!({
            "displayName": "Werewolf Server",
            "worldName": "Werewolf",
            "maxPlayers": 10
        }))
        .send()
        .await
        .map_err(|error| format!("中央サーバーへ登録できませんでした: {error}"))?
        .error_for_status()
        .map_err(|error| format!("中央サーバーが登録を拒否しました: {error}"))?
        .json::<RegistrationResponse>()
        .await
        .map_err(|error| format!("中央サーバーの応答を解析できませんでした: {error}"))?;
    Ok(Session {
        id: registration.id,
        token: registration.token,
        endpoint,
    })
}

async fn heartbeat(session: &Session) -> Result<(), String> {
    let (status, mode, host_name, host_port) = match &session.endpoint {
        Some(endpoint) => (
            "online",
            endpoint.mode,
            Some(endpoint.host_name.as_str()),
            Some(endpoint.host_port),
        ),
        None => ("starting", "pending", None, None),
    };
    reqwest::Client::new()
        .put(format!("{DIRECTORY_URL}/{}/heartbeat", session.id))
        .bearer_auth(&session.token)
        .json(&serde_json::json!({
            "playerCount": 0,
            "maxPlayers": 10,
            "status": status,
            "connectionMode": mode,
            "hostName": host_name,
            "hostPort": host_port
        }))
        .send()
        .await
        .map_err(|error| format!("heartbeatを送信できませんでした: {error}"))?
        .error_for_status()
        .map_err(|error| format!("heartbeatが拒否されました: {error}"))?;
    Ok(())
}

fn spawn_heartbeat(state: NetworkState) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let session = state.0.lock().ok().and_then(|value| value.clone());
            let Some(session) = session else { break };
            let _ = heartbeat(&session).await;
        }
    });
}

fn spawn_relay(state: NetworkState, session: Session) {
    tauri::async_runtime::spawn(async move {
        loop {
            if relay_once(state.clone(), session.clone()).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            let active = state
                .0
                .lock()
                .ok()
                .and_then(|value| value.as_ref().map(|value| value.id == session.id))
                .unwrap_or(false);
            if !active {
                break;
            }
        }
    });
}

async fn relay_once(state: NetworkState, session: Session) -> Result<(), String> {
    let url = format!(
        "wss://mc-werewolf.com/api/network/v1/servers/{}/relay",
        session.id
    );
    let mut request = url
        .into_client_request()
        .map_err(|error| format!("中継URLを作成できませんでした: {error}"))?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", session.token))
            .map_err(|error| format!("中継認証を作成できませんでした: {error}"))?,
    );
    let (websocket, _) = connect_async(request)
        .await
        .map_err(|error| format!("中央中継へ接続できませんでした: {error}"))?;
    let (mut writer, mut reader) = websocket.split();
    let (outbound, mut outbound_receiver) = mpsc::channel::<Vec<u8>>(256);
    let writer_task = tauri::async_runtime::spawn(async move {
        while let Some(frame) = outbound_receiver.recv().await {
            writer
                .send(Message::Binary(frame.into()))
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok::<(), String>(())
    });
    let mut clients: HashMap<String, std::sync::Arc<TokioUdpSocket>> = HashMap::new();
    while let Some(message) = reader.next().await {
        match message.map_err(|error| format!("中継接続が切断されました: {error}"))? {
            Message::Text(text) => {
                #[derive(Deserialize)]
                struct Ready {
                    #[serde(rename = "type")]
                    kind: String,
                    #[serde(rename = "hostName")]
                    host_name: String,
                    port: u16,
                }
                let ready: Ready = serde_json::from_str(text.as_ref())
                    .map_err(|error| format!("中継準備通知を解析できませんでした: {error}"))?;
                if ready.kind == "ready" {
                    let endpoint = Endpoint {
                        host_name: ready.host_name,
                        host_port: ready.port,
                        mode: "relay",
                    };
                    let updated = {
                        let mut guard = state
                            .0
                            .lock()
                            .map_err(|_| "ネットワーク状態を更新できませんでした")?;
                        if let Some(current) = guard.as_mut() {
                            if current.id == session.id {
                                current.endpoint = Some(endpoint);
                                Some(current.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(updated) = updated {
                        heartbeat(&updated).await?;
                    }
                }
            }
            Message::Binary(frame) => {
                let (remote_address, payload) = decode_relay_frame(&frame)?;
                let socket = if let Some(socket) = clients.get(&remote_address) {
                    socket.clone()
                } else {
                    let socket =
                        std::sync::Arc::new(TokioUdpSocket::bind("0.0.0.0:0").await.map_err(
                            |error| format!("ローカルUDPを作成できませんでした: {error}"),
                        )?);
                    socket
                        .connect((Ipv4Addr::LOCALHOST, BDS_PORT))
                        .await
                        .map_err(|error| format!("ローカルBDSへ接続できませんでした: {error}"))?;
                    let receive_socket = socket.clone();
                    let receive_address = remote_address.clone();
                    let receive_outbound = outbound.clone();
                    tauri::async_runtime::spawn(async move {
                        let mut buffer = vec![0_u8; max_datagram_size()];
                        while let Ok(size) = receive_socket.recv(&mut buffer).await {
                            if receive_outbound
                                .send(encode_relay_frame(&receive_address, &buffer[..size]))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                    clients.insert(remote_address.clone(), socket.clone());
                    socket
                };
                socket
                    .send(payload)
                    .await
                    .map_err(|error| format!("BDSへUDPを転送できませんでした: {error}"))?;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    drop(outbound);
    writer_task.await.map_err(|error| error.to_string())??;
    Err("中央中継が終了しました".to_owned())
}

fn max_datagram_size() -> usize {
    65_535
}

fn encode_relay_frame(address: &str, payload: &[u8]) -> Vec<u8> {
    let address = address.as_bytes();
    let mut frame = Vec::with_capacity(2 + address.len() + payload.len());
    frame.extend_from_slice(&(address.len() as u16).to_be_bytes());
    frame.extend_from_slice(address);
    frame.extend_from_slice(payload);
    frame
}

fn decode_relay_frame(frame: &[u8]) -> Result<(String, &[u8]), String> {
    if frame.len() < 2 {
        return Err("中継フレームが短すぎます".to_owned());
    }
    let address_length = u16::from_be_bytes([frame[0], frame[1]]) as usize;
    if address_length == 0 || frame.len() < 2 + address_length {
        return Err("中継フレームのアドレス長が不正です".to_owned());
    }
    let address = std::str::from_utf8(&frame[2..2 + address_length])
        .map_err(|error| format!("中継アドレスが不正です: {error}"))?;
    Ok((address.to_owned(), &frame[2 + address_length..]))
}

fn local_ip_for_gateway(gateway: SocketAddr) -> Result<IpAddr, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|error| format!("ローカルネットワークを確認できませんでした: {error}"))?;
    socket
        .connect(gateway)
        .map_err(|error| format!("ルーターへ接続できませんでした: {error}"))?;
    socket
        .local_addr()
        .map(|address| address.ip())
        .map_err(|error| format!("ローカルIPを取得できませんでした: {error}"))
}

#[cfg(windows)]
fn request_firewall_rule() -> Result<bool, String> {
    let arguments = format!(
        "advfirewall firewall add rule name=\"Werewolf BDS UDP {BDS_PORT}\" dir=in action=allow protocol=UDP localport={BDS_PORT}"
    );
    let script = format!(
        "Start-Process -FilePath netsh.exe -Verb RunAs -ArgumentList '{}' -Wait",
        arguments.replace('\'', "''")
    );
    let success = Command::new("powershell.exe")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .status()
        .map_err(|error| format!("Firewall設定を開始できませんでした: {error}"))?
        .success();
    if success {
        Ok(true)
    } else {
        Err("Windows Firewall設定がキャンセルまたは失敗しました".to_owned())
    }
}

#[cfg(not(windows))]
fn request_firewall_rule() -> Result<bool, String> {
    Ok(false)
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => !(ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_public_and_non_public_addresses() {
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        for address in ["127.0.0.1", "192.168.1.10", "10.0.0.1", "100.64.0.1"] {
            assert!(!is_public_ip(address.parse().unwrap()));
        }
    }

    #[test]
    fn relay_frame_round_trip() {
        let encoded = encode_relay_frame("203.0.113.10:54321", &[1, 2, 3]);
        let (address, payload) = decode_relay_frame(&encoded).unwrap();
        assert_eq!(address, "203.0.113.10:54321");
        assert_eq!(payload, &[1, 2, 3]);
    }
}
