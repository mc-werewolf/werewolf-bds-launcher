use igd_next::{aio::tokio::search_gateway, PortMappingProtocol, SearchOptions};
use serde::Serialize;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    process::Command,
    time::Duration,
};

const BDS_PORT: u16 = 19132;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub public_address: String,
    pub local_address: String,
    pub port: u16,
    pub firewall_requested: bool,
    pub upnp_mapped: bool,
    pub warning: Option<String>,
}

pub async fn publish() -> Result<PublishResult, String> {
    let firewall_requested = request_firewall_rule()?;
    let gateway = search_gateway(SearchOptions {
        timeout: Some(Duration::from_secs(10)),
        single_search_timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    })
    .await
    .map_err(|error| format!("UPnP対応ルーターを検出できませんでした: {error}"))?;
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
    let warning = (!is_public_ip(public_ip)).then(|| {
        "CGNATまたは二重ルーターの可能性があります。中継サーバーか上位ルーターの設定が必要です。".to_owned()
    });
    Ok(PublishResult {
        public_address: format!("{public_ip}:{BDS_PORT}"),
        local_address: local_address.to_string(),
        port: BDS_PORT,
        firewall_requested,
        upnp_mapped: true,
        warning,
    })
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
}
