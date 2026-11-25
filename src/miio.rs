use crate::{Result, cloud::PowerProperty};
use aes::Aes128;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use if_addrs::IfAddr;
use md5::{Digest, Md5};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const PORT: u16 = 54321;
const TIMEOUT: Duration = Duration::from_millis(1200);
const HEADER: usize = 32;

pub struct Miio {
    socket: UdpSocket,
    token: [u8; 16],
    did: u32,
    stamp: u32,
    sequence: u32,
}

pub fn local_ip(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

fn same_subnet(source: Ipv4Addr, target: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    let source = u32::from(source);
    let target = u32::from(target);
    let mask = u32::from(netmask);
    let network = source & mask;
    source != target
        && mask != 0
        && target & mask == network
        && target != network
        && target != (network | !mask)
}

impl Miio {
    pub fn connect(ip: Ipv4Addr, did: u32, token: [u8; 16]) -> Result<Self> {
        if !local_ip(ip) {
            return Err("设备没有可用的局域网 IPv4 地址".into());
        }
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket
            .connect(SocketAddrV4::new(ip, PORT))
            .map_err(|err| format!("设备 {ip} 不在当前可用的局域网路由上: {err}"))?;
        let source = socket.local_addr()?.ip();
        let on_link = if_addrs::get_if_addrs()?.into_iter().any(|iface| {
            iface.is_oper_up()
                && !iface.is_p2p()
                && matches!(iface.addr, IfAddr::V4(addr)
                    if source == IpAddr::V4(addr.ip) && same_subnet(addr.ip, ip, addr.netmask))
        });
        if !on_link {
            return Err(format!("设备 {ip} 不在当前连接的 IPv4 局域网网段内").into());
        }
        Self::handshake(socket, did, token)
    }

    fn handshake(socket: UdpSocket, did: u32, token: [u8; 16]) -> Result<Self> {
        let mut hello = [0xffu8; HEADER];
        hello[..4].copy_from_slice(&[0x21, 0x31, 0, HEADER as u8]);
        socket.set_read_timeout(Some(TIMEOUT))?;
        socket.set_write_timeout(Some(TIMEOUT))?;
        socket.send(&hello)?;
        let mut reply = [0u8; 2048];
        let size = socket.recv(&mut reply).map_err(|err| {
            if timed_out(&err) {
                "同网段设备未响应 miIO 握手，请检查设备、IP 或网络隔离".to_string()
            } else {
                format!("局域网 miIO 握手失败: {err}")
            }
        })?;
        let (reply_did, stamp) = parse_header(&reply[..size])?;
        if size != HEADER || reply_did != did {
            return Err("该 IP 响应的 miIO 设备与已登记的设备 ID 不符".into());
        }
        let random = getrandom::u32().map_err(|err| format!("无法生成 miIO 请求 ID: {err}"))?;
        Ok(Self {
            socket,
            token,
            did,
            stamp,
            sequence: random & 0x3fff_ffff,
        })
    }

    pub fn property(
        &mut self,
        property: PowerProperty,
        value: Option<bool>,
    ) -> Result<Option<bool>> {
        self.sequence = (self.sequence + 1) & 0x3fff_ffff;
        let id = self.sequence;
        let property_id = format!("{}-{}", property.siid, property.piid);
        let mut query = json!({"did": property_id, "siid": property.siid, "piid": property.piid});
        let method = if let Some(value) = value {
            query["value"] = json!(value);
            "set_properties"
        } else {
            "get_properties"
        };
        let request = json!({"id": id, "method": method, "params": [query]});
        let packet = encode(self.did, self.stamp.wrapping_add(1), &self.token, &request)?;
        self.socket.send(&packet)?;
        let deadline = Instant::now() + TIMEOUT;
        let mut buffer = [0u8; 8192];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("未收到设备的有效局域网回复".into());
            }
            self.socket.set_read_timeout(Some(remaining))?;
            let size = self.socket.recv(&mut buffer).map_err(|err| {
                if timed_out(&err) {
                    "未收到设备的有效局域网回复，请确认设备支持本地控制且 token 有效".to_string()
                } else {
                    format!("局域网读取失败: {err}")
                }
            })?;
            // 设备可在命令回复前再发一个握手包。
            if size == HEADER {
                continue;
            }
            let (stamp, payload) = decode(&mut buffer[..size], self.did, &self.token)?;
            if payload.id != id {
                continue;
            }
            self.stamp = stamp;
            if let Some(error) = payload.error {
                return Err(format!("设备拒绝局域网请求（状态码 {}）", error.code).into());
            }
            let [item] = payload.result.as_deref().ok_or("局域网回复缺少属性结果")?
            else {
                return Err("局域网回复的属性结果数量异常".into());
            };
            if item.did != property_id || item.siid != property.siid || item.piid != property.piid {
                return Err("局域网回复的属性与请求不符".into());
            }
            if item.code != 0 && !(value.is_some() && item.code == 1) {
                return Err(format!("设备拒绝局域网属性请求（状态码 {}）", item.code).into());
            }
            return Ok(item.value);
        }
    }
}

fn timed_out(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

#[derive(Deserialize)]
struct Reply {
    id: u32,
    result: Option<Vec<PropertyReply>>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i32,
}

#[derive(Deserialize)]
struct PropertyReply {
    did: String,
    siid: i32,
    piid: i32,
    code: i32,
    value: Option<bool>,
}

fn parse_header(packet: &[u8]) -> Result<(u32, u32)> {
    if packet.len() < HEADER || &packet[..2] != b"\x21\x31" {
        return Err("无效的 miIO 数据包头".into());
    }
    let length = u16::from_be_bytes(packet[2..4].try_into()?) as usize;
    if length != packet.len() {
        return Err("miIO 数据包长度不符".into());
    }
    Ok((
        u32::from_be_bytes(packet[8..12].try_into()?),
        u32::from_be_bytes(packet[12..16].try_into()?),
    ))
}

// miIO 设备使用由 token 派生的 AES-CBC 密钥和报文校验值。
fn key_iv(token: &[u8; 16]) -> ([u8; 16], [u8; 16]) {
    let key: [u8; 16] = Md5::digest(token).into();
    let mut digest = Md5::new();
    digest.update(key);
    digest.update(token);
    (key, digest.finalize().into())
}

fn checksum(packet: &[u8], token: &[u8; 16]) -> [u8; 16] {
    let mut digest = Md5::new();
    digest.update(&packet[..16]);
    digest.update(token);
    digest.update(&packet[HEADER..]);
    digest.finalize().into()
}

fn encode(did: u32, stamp: u32, token: &[u8; 16], request: &Value) -> Result<Vec<u8>> {
    let (key, iv) = key_iv(token);
    let mut payload = serde_json::to_vec(request)?;
    payload.push(0);
    let encrypted = cbc::Encryptor::<Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(&payload);
    let mut packet = vec![0u8; HEADER];
    packet[..2].copy_from_slice(b"\x21\x31");
    packet[2..4].copy_from_slice(&u16::try_from(HEADER + encrypted.len())?.to_be_bytes());
    packet[8..12].copy_from_slice(&did.to_be_bytes());
    packet[12..16].copy_from_slice(&stamp.to_be_bytes());
    packet.extend_from_slice(&encrypted);
    let hash = checksum(&packet, token);
    packet[16..HEADER].copy_from_slice(&hash);
    Ok(packet)
}

fn decode(packet: &mut [u8], expected_did: u32, token: &[u8; 16]) -> Result<(u32, Reply)> {
    let (did, stamp) = parse_header(packet)?;
    if did != expected_did || packet[16..HEADER] != checksum(packet, token) {
        return Err("局域网回复未通过设备身份校验，请检查设备 IP 和 token".into());
    }
    let (key, iv) = key_iv(token);
    let data = cbc::Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut packet[HEADER..])
        .map_err(|_| "无法解密 miIO 回复")?;
    let end = data
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |at| at + 1);
    let reply = serde_json::from_slice(&data[..end]).map_err(|_| "无法解析 miIO 回复")?;
    Ok((stamp, reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

    #[test]
    fn subnet_rejects_outside_hosts_and_broadcast_addresses() {
        let source = Ipv4Addr::new(192, 168, 7, 12);
        let mask = Ipv4Addr::new(255, 255, 255, 192);
        for (host, accepted) in [
            (11, true),
            (12, false),
            (0, false),
            (63, false),
            (76, false),
        ] {
            assert_eq!(
                same_subnet(source, Ipv4Addr::new(192, 168, 7, host), mask),
                accepted
            );
        }
        assert!(!local_ip(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn packet_matches_independent_aes_and_checksum_vector() {
        let packet = encode(
            123,
            456,
            &TOKEN,
            &json!({"id":7,"method":"get_properties","params":[{"did":"2-1","siid":2,"piid":1}]}),
        )
        .unwrap();
        let expected = concat!(
            "21310070000000000000007b000001c8cfb2917e7ab12195c9823896",
            "5bb77fb7a23ac1cafc3ef227362c90c45ad4861d42885e9c62e466af",
            "7fbeb7ba0726df8202f77a887ba836a8ee15ee2b3dbfb920c5ca52d2",
            "3c4991b96cbe2c7749d921ecf281d276a2b0acbe5746b19bfdefd6f9",
        );
        let expected: Vec<_> = (0..expected.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&expected[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(packet, expected);
    }

    #[test]
    fn local_udp_power_round_trip() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        server.set_read_timeout(Some(TIMEOUT)).unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket.connect(server.local_addr().unwrap()).unwrap();
        let responder = std::thread::spawn(move || {
            let mut raw = [0; 2048];
            let (size, peer) = server.recv_from(&mut raw).unwrap();
            assert_eq!(size, HEADER);
            assert_eq!(&raw[..4], &[0x21, 0x31, 0, HEADER as u8]);
            let mut hello = [0; HEADER];
            hello[..4].copy_from_slice(&raw[..4]);
            hello[8..12].copy_from_slice(&123u32.to_be_bytes());
            hello[12..16].copy_from_slice(&456u32.to_be_bytes());
            server.send_to(&hello, peer).unwrap();
            let mut current = true;
            for (step, method) in ["get_properties", "set_properties", "get_properties"]
                .into_iter()
                .enumerate()
            {
                let (size, peer) = server.recv_from(&mut raw).unwrap();
                let packet = &mut raw[..size];
                assert_eq!(parse_header(packet).unwrap().0, 123);
                assert_eq!(packet[16..HEADER], checksum(packet, &TOKEN));
                let (key, iv) = key_iv(&TOKEN);
                let decoded = cbc::Decryptor::<Aes128>::new(&key.into(), &iv.into())
                    .decrypt_padded_mut::<Pkcs7>(&mut packet[HEADER..])
                    .unwrap();
                let data: Value =
                    serde_json::from_slice(decoded.strip_suffix(&[0]).unwrap()).unwrap();
                assert_eq!(data["method"], method);
                assert_eq!(data["params"][0]["did"], "2-1");
                if step == 1 {
                    assert_eq!(data["params"][0]["value"], false);
                    current = false;
                }
                let reply = json!({
                    "id":data["id"],
                    "result":[{"did":"2-1","siid":2,"piid":1,"code":0,"value":current}]
                });
                let response = encode(123, 457 + step as u32, &TOKEN, &reply).unwrap();
                server.send_to(&response, peer).unwrap();
            }
        });
        let mut client = Miio::handshake(socket, 123, TOKEN).unwrap();
        let property = PowerProperty { siid: 2, piid: 1 };
        assert_eq!(client.property(property, None).unwrap(), Some(true));
        client.property(property, Some(false)).unwrap();
        assert_eq!(client.property(property, None).unwrap(), Some(false));
        responder.join().unwrap();
    }
}
