use crate::{
    Result,
    cloud::{Client, PowerProperty, PowerState, Transport},
    miio::{Miio, local_ip},
    session,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

#[derive(Deserialize, Serialize)]
pub struct DeviceInfo {
    pub did: String,
    pub name: String,
    pub model: String,
    pub ip: Ipv4Addr,
}

#[derive(Deserialize, Serialize)]
pub struct Device {
    #[serde(flatten)]
    pub info: DeviceInfo,
    property: PowerProperty,
    token: [u8; 16],
}

impl Device {
    pub fn connect(&self) -> Result<Miio> {
        Miio::connect(self.info.ip, self.info.did.parse()?, self.token)
    }

    pub fn read<'a>(&self, id: &'a str, client: &mut Miio) -> Result<PowerState<'a>> {
        Ok(PowerState {
            device_id: id,
            power: client
                .property(self.property, None)?
                .ok_or("局域网回复缺少开关状态")?,
            transport: Transport::Lan,
        })
    }

    pub fn write<'a>(&self, id: &'a str, client: &mut Miio, on: bool) -> Result<PowerState<'a>> {
        client
            .property(self.property, Some(on))
            .map_err(|err| format!("局域网写入未获确认，请先读取状态再决定是否重试: {err}"))?;
        for attempt in 0..3 {
            let state = self
                .read(id, client)
                .map_err(|err| format!("设备已接受局域网写入，读回失败: {err}"))?;
            if state.power == on {
                return Ok(state);
            }
            if attempt < 2 {
                thread::sleep(Duration::from_millis(300));
            }
        }
        Err("设备已接受局域网写入，但读回状态未达到目标值".into())
    }
}

pub fn import(client: &Client, id: &str) -> Result<DeviceInfo> {
    if !valid_id(id) {
        return Err("当前只支持具有数字设备 ID 的 IPv4 miIO 设备".into());
    }
    let device = client.device(id)?;
    if matches!(device.pid, Some(6 | 16)) {
        return Err("该设备通过蓝牙或蓝牙 Mesh 接入，需要支持本地转发的网关".into());
    }
    let ip = device
        .local_ip
        .as_deref()
        .and_then(|ip| ip.parse().ok())
        .filter(|&ip| local_ip(ip))
        .ok_or("米家未提供该设备的局域网 IPv4 地址")?;
    let token = decode_token(
        device
            .token
            .as_deref()
            .ok_or("米家未提供该设备的本地 token")?,
    )?;
    let property = client.resolve_power(&device, true)?;
    let info = DeviceInfo {
        did: device.id,
        name: device.name,
        model: device.model,
        ip,
    };
    let device = Device {
        info,
        property,
        token,
    };
    session::save_private_json(&directory()?.join(format!("{id}.json")), &device)?;
    Ok(device.info)
}

pub fn load(id: &str) -> Result<Option<Device>> {
    if !valid_id(id) {
        return Ok(None);
    }
    let path = directory()?.join(format!("{id}.json"));
    match File::open(&path) {
        Ok(file) => read_file(file, id).map(Some),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn list() -> Result<Vec<DeviceInfo>> {
    let entries = match fs::read_dir(directory()?) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut devices = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let Some(id) = path
            .file_stem()
            .and_then(|id| id.to_str())
            .filter(|id| valid_id(id))
        else {
            continue;
        };
        if path.extension() == Some("json".as_ref()) {
            devices.push(read_file(File::open(&path)?, id)?.info);
        }
    }
    devices.sort_by(|a, b| a.did.cmp(&b.did));
    Ok(devices)
}

fn read_file(file: File, id: &str) -> Result<Device> {
    let invalid = || format!("设备 {id} 的局域网配置无效，请重新导入");
    let device: Device = serde_json::from_reader(file).map_err(|_| invalid())?;
    if device.info.did != id
        || !local_ip(device.info.ip)
        || !usable_token(&device.token)
        || device.property.siid <= 0
        || device.property.piid <= 0
    {
        return Err(invalid().into());
    }
    Ok(device)
}

fn valid_id(id: &str) -> bool {
    id.parse::<u32>()
        .is_ok_and(|did| did != 0 && did.to_string() == id)
}

fn decode_token(text: &str) -> Result<[u8; 16]> {
    const INVALID: &str = "米家未提供有效的本地 miIO token";
    let mut token = [0; 16];
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(INVALID.into());
    }
    for (i, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)?;
    }
    if !usable_token(&token) {
        return Err(INVALID.into());
    }
    Ok(token)
}

fn usable_token(token: &[u8; 16]) -> bool {
    *token != [0; 16] && *token != [0xff; 16]
}

fn directory() -> Result<PathBuf> {
    Ok(session::directory()?.join(Path::new("lan")))
}
