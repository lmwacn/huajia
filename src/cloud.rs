use crate::{Result, session::Session};
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::read::GzDecoder;
use rc4::{KeyInit, Rc4, StreamCipher, consts::U32};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::collections::HashSet;
use std::io::Read;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use ureq::http::header;
use url::form_urlencoded;

const CLOUD: &str = "https://api.io.mi.com/app";
const SPEC: &str = "https://miot-spec.org/miot-spec-v2/instance";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const BODY_LIMIT: u64 = 8 << 20;

#[derive(Deserialize, Serialize)]
pub struct Device {
    #[serde(rename = "did")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(
        default,
        rename = "spec_type",
        skip_serializing_if = "String::is_empty"
    )]
    pub spec: String,
    #[serde(rename = "isOnline", skip_serializing_if = "Option::is_none")]
    pub online: Option<bool>,
    #[serde(default, rename = "localip", skip_serializing)]
    pub local_ip: Option<String>,
    #[serde(default, skip_serializing)]
    pub token: Option<String>,
    #[serde(default, skip_serializing)]
    pub pid: Option<i32>,
}

#[derive(Deserialize)]
struct Page {
    list: Vec<Device>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_start_did: String,
}

#[derive(Deserialize)]
struct Spec {
    services: Vec<Service>,
}

#[derive(Deserialize)]
struct Service {
    iid: i32,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    properties: Vec<Property>,
}

#[derive(Deserialize)]
struct Property {
    iid: i32,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    access: Vec<String>,
}

#[derive(Clone, Serialize)]
struct Query<'a> {
    did: &'a str,
    #[serde(flatten)]
    property: PowerProperty,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<bool>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub struct PowerProperty {
    pub siid: i32,
    pub piid: i32,
}

#[derive(Deserialize)]
struct PropertyReply {
    did: String,
    siid: i32,
    piid: i32,
    code: i32,
    value: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PropertyResults {
    List(Vec<PropertyReply>),
    Wrapped { params: Vec<PropertyReply> },
    Single(PropertyReply),
}

#[derive(Serialize)]
pub struct PowerState<'a> {
    pub device_id: &'a str,
    pub power: bool,
    pub transport: Transport,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Lan,
    Cloud,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::Cloud => "cloud",
        }
    }
}

pub struct Client {
    session: Session,
    agent: ureq::Agent,
    cloud: String,
    spec: String,
    deadline: Instant,
}

impl Client {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            agent: ureq::Agent::config_builder()
                .max_redirects(0)
                .build()
                .into(),
            cloud: CLOUD.to_string(),
            spec: SPEC.to_string(),
            deadline: Instant::now() + Duration::from_secs(90),
        }
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        let mut devices = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = String::new();
        loop {
            let mut input = json!({"dids": [], "limit": 200, "get_split_device": true});
            if !cursor.is_empty() {
                input["start_did"] = json!(cursor);
            }
            let page: Page = self.post("/v2/home/device_list_page", &input)?;
            devices.extend(page.list);
            if !page.has_more {
                break;
            }
            if page.next_start_did.is_empty() || !seen.insert(page.next_start_did.clone()) {
                return Err("米家云端未返回有效的设备分页游标".into());
            }
            cursor = page.next_start_did;
        }
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(devices)
    }

    pub fn get_power<'a>(&self, id: &'a str) -> Result<PowerState<'a>> {
        let query = self.power_property(id, false)?;
        self.read_power(&query)
    }

    pub fn set_power<'a>(&self, id: &'a str, on: bool) -> Result<PowerState<'a>> {
        let query = self.power_property(id, true)?;
        let mut write = query.clone();
        write.value = Some(on);
        self.property_request("/miotspec/prop/set", &write, false)?;

        // 云端可能先确认写入，再更新设备状态。仅重试读回。
        for attempt in 0..4 {
            let state = self
                .read_power(&query)
                .map_err(|err| format!("云端已接受写入，读回失败: {err}"))?;
            if state.power == on {
                return Ok(state);
            }
            if attempt < 3 {
                thread::sleep(Duration::from_secs(1).min(self.remaining()));
            }
        }
        Err("云端已接受写入，但设备状态未达到目标值".into())
    }

    pub fn device(&self, id: &str) -> Result<Device> {
        self.devices()?
            .into_iter()
            .find(|d| d.id == id)
            .ok_or_else(|| format!("找不到设备 {id:?}").into())
    }

    fn power_property<'a>(&self, id: &'a str, write: bool) -> Result<Query<'a>> {
        Ok(Query {
            did: id,
            property: self.resolve_power(&self.device(id)?, write)?,
            value: None,
        })
    }

    pub fn resolve_power(&self, device: &Device, write: bool) -> Result<PowerProperty> {
        let kind = device
            .spec
            .strip_prefix("urn:miot-spec-v2:device:")
            .and_then(|name| name.split_once(':'))
            .map(|(kind, _)| kind);
        let service_kind = match kind {
            Some("light") => "urn:miot-spec-v2:service:light:",
            Some("fan") => "urn:miot-spec-v2:service:fan:",
            Some("outlet") => "urn:miot-spec-v2:service:switch:",
            _ => return Err("当前只支持带 MIoT 规格的灯具、风扇和插座".into()),
        };
        let raw = read_response(
            self.agent
                .get(&self.spec)
                .query("type", &device.spec)
                .config()
                .timeout_global(Some(self.remaining().min(REQUEST_TIMEOUT)))
                .build()
                .call()?,
        )?;
        let spec: Spec = serde_json::from_slice(&raw).map_err(|_| "无法读取设备的 MIoT 规格")?;
        let mut matches = spec.services.iter().flat_map(|service| {
            service.properties.iter().filter_map(move |prop| {
                (service.iid > 0
                    && prop.iid > 0
                    && service.kind.starts_with(service_kind)
                    && prop.kind.starts_with("urn:miot-spec-v2:property:on:")
                    && prop.format == "bool"
                    && prop.access.iter().any(|access| access == "read")
                    && (!write || prop.access.iter().any(|access| access == "write")))
                .then_some(PowerProperty {
                    siid: service.iid,
                    piid: prop.iid,
                })
            })
        });
        let target = matches.next().ok_or("设备规格不支持所需的开关操作")?;
        if matches.next().is_some() {
            return Err("设备存在多个开关属性，无法确定要操作的开关".into());
        }
        Ok(target)
    }

    fn read_power<'a>(&self, query: &Query<'a>) -> Result<PowerState<'a>> {
        let reply = self.property_request("/miotspec/prop/get", query, true)?;
        Ok(PowerState {
            device_id: query.did,
            power: reply.value.ok_or("米家云端未返回开关状态")?,
            transport: Transport::Cloud,
        })
    }

    fn property_request(&self, path: &str, query: &Query<'_>, read: bool) -> Result<PropertyReply> {
        let mut input = json!({"params": [query]});
        if read {
            input["datasource"] = json!(1);
        }
        let results: PropertyResults = self.post(path, &input)?;
        let item = match results {
            PropertyResults::Single(item) => item,
            PropertyResults::List(items) | PropertyResults::Wrapped { params: items } => {
                let [item]: [PropertyReply; 1] =
                    items.try_into().map_err(|_| "米家云端未确认目标属性")?;
                item
            }
        };
        if item.did != query.did
            || item.siid != query.property.siid
            || item.piid != query.property.piid
        {
            return Err("米家云端未确认目标属性".into());
        }
        if item.code != 0 && (read || item.code != 1) {
            return Err(format!("设备未接受属性请求，状态码 {}", item.code).into());
        }
        Ok(item)
    }

    fn post<T: DeserializeOwned>(&self, path: &str, input: &Value) -> Result<T> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce[..8]).map_err(|err| format!("无法生成请求随机数: {err}"))?;
        let minutes = (SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 60) as u32;
        nonce[8..].copy_from_slice(&minutes.to_be_bytes());
        let (form, key) = signed_form(
            path,
            &serde_json::to_vec(input)?,
            &self.session.ssecurity,
            &nonce,
        )?;
        let s = &self.session;
        let cookies = [
            ("userId", s.user_id.as_str()),
            ("cUserId", s.c_user_id.as_str()),
            ("serviceToken", s.service_token.as_str()),
            ("yetAnotherServiceToken", s.service_token.as_str()),
            ("locale", "zh_CN"),
            ("countryCode", "CN"),
            ("channel", "MI_APP_STORE"),
            ("timezone_id", "Asia/Shanghai"),
            ("timezone", "GMT%2B08:00"),
            ("is_daylight", "0"),
            ("dst_offset", "0"),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
        let response = self
            .agent
            .post(format!("{}{path}", self.cloud))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::COOKIE, cookies)
            .header("X-Xiaomi-Protocal-Flag-Cli", "PROTOCAL-HTTP2")
            .header("MIOT-ACCEPT-ENCODING", "GZIP")
            .header("MIOT-REQUEST-PAGE", "none")
            .header("MIOT-ENCRYPT-ALGORITHM", "ENCRYPT-RC4")
            .config()
            .timeout_global(Some(self.remaining().min(REQUEST_TIMEOUT)))
            .build()
            .send(form)
            .map_err(|err| match err {
                ureq::Error::StatusCode(401) => "米家授权无效，请重新登录".into(),
                _ => Box::<dyn std::error::Error>::from(err),
            })?;
        let gzip = response
            .headers()
            .get("MIOT-CONTENT-ENCODING")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("gzip"));
        let raw = decode_cloud(&read_response(response)?, gzip, &key)?;
        #[derive(Deserialize)]
        struct Envelope<T> {
            code: i32,
            result: Option<T>,
        }
        let envelope: Envelope<Value> =
            serde_json::from_slice(&raw).map_err(|_| "米家云端响应缺少状态码")?;
        match envelope.code {
            0 => Ok(serde_json::from_value(
                envelope.result.ok_or("米家云端响应缺少结果")?,
            )?),
            code @ (2 | 3 | 401) => Err(format!("米家授权无效（{code}），请重新登录").into()),
            code => Err(format!("米家云端返回状态码 {code}").into()),
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

fn read_response(mut response: ureq::http::Response<ureq::Body>) -> Result<Vec<u8>> {
    if response.status() != ureq::http::StatusCode::OK {
        return Err(format!("米家接口返回 HTTP {}", response.status().as_u16()).into());
    }
    Ok(response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()?)
}

// 小米云端兼容协议在 HTTPS 内使用 RC4，并丢弃最初 1024 字节的密钥流。
fn cipher(key: &[u8; 32]) -> Rc4<U32> {
    let mut stream = Rc4::<U32>::new(key.into());
    stream.apply_keystream(&mut [0u8; 1024]);
    stream
}

fn signed_form(
    path: &str,
    payload: &[u8],
    secret: &str,
    nonce: &[u8; 12],
) -> Result<(String, [u8; 32])> {
    let secret_bytes = STANDARD
        .decode(secret)
        .map_err(|_| "米家授权密钥无效，请重新登录")?;
    if secret_bytes.is_empty() {
        return Err("米家授权密钥无效，请重新登录".into());
    }
    let mut digest = Sha256::new();
    digest.update(secret_bytes);
    digest.update(nonce);
    let key: [u8; 32] = digest.finalize().into();
    let key_text = STANDARD.encode(key);
    let sign =
        |params: &str| STANDARD.encode(Sha1::digest(format!("POST&{path}&{params}&{key_text}")));

    let plain_hash = sign(&format!("data={}", std::str::from_utf8(payload)?));
    let mut stream = cipher(&key);
    let mut encrypt = |plain: &[u8]| {
        let mut raw = plain.to_vec();
        stream.apply_keystream(&mut raw);
        STANDARD.encode(raw)
    };
    let data = encrypt(payload);
    let hash = encrypt(plain_hash.as_bytes());
    let signature = sign(&format!("data={data}&rc4_hash__={hash}"));
    let form = form_urlencoded::Serializer::new(String::new())
        .append_pair("data", &data)
        .append_pair("rc4_hash__", &hash)
        .append_pair("signature", &signature)
        .append_pair("_nonce", &STANDARD.encode(nonce))
        .append_pair("ssecurity", secret)
        .finish();
    Ok((form, key))
}

fn decode_cloud(raw: &[u8], gzip: bool, key: &[u8; 32]) -> Result<Vec<u8>> {
    let raw = raw.trim_ascii();
    if raw.starts_with(b"{") {
        return Ok(raw.to_vec());
    }
    let mut plain = STANDARD
        .decode(raw)
        .map_err(|_| "米家云端返回了无法识别的响应")?;
    cipher(key).apply_keystream(&mut plain);
    if gzip || plain.starts_with(b"\x1f\x8b") {
        let mut output = Vec::new();
        GzDecoder::new(plain.as_slice())
            .take(BODY_LIMIT + 1)
            .read_to_end(&mut output)?;
        if output.len() as u64 > BODY_LIMIT {
            return Err("米家云端响应过大".into());
        }
        return Ok(output);
    }
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "c2VjcmV0";
    const LAMP_SPEC: &str = "urn:miot-spec-v2:device:light:0000A001:opple-ceiling:1";

    #[test]
    fn signed_form_matches_reference() {
        let nonce = std::array::from_fn(|i| i as u8);
        let (form, _) =
            signed_form("/miotspec/prop/get", br#"{"datasource":1}"#, SECRET, &nonce).unwrap();
        let fields: std::collections::HashMap<_, _> =
            form_urlencoded::parse(form.as_bytes()).collect();
        assert_eq!(fields["data"], "ct7B0J61mnneqIVzf7rhcQ==");
        assert_eq!(
            fields["rc4_hash__"],
            "Rw3NhjNavvr4RRAzsYYhJhaH8yqUmvuNxBQcLw=="
        );
        assert_eq!(fields["signature"], "glKu9y6mIyISwyZJNjvpvd8p4Ow=");
    }

    #[test]
    fn light_power_round_trip() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let responder = thread::spawn(move || {
            let mut on = true;
            let mut writes = 0;
            for _ in 0..9 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                if request.url().starts_with("/spec?") {
                    assert!(!request.headers().iter().any(|h| h.field.equiv("Cookie")));
                    request.respond(tiny_http::Response::from_string(
                        r#"{"services":[{"iid":7,"type":"urn:miot-spec-v2:service:light:1","properties":[{"iid":9,"type":"urn:miot-spec-v2:property:on:1","format":"bool","access":["read","write"]}]}]}"#,
                    )).unwrap();
                    continue;
                }
                assert!(request.headers().iter().any(|h| {
                    h.field.equiv("Cookie") && h.value.as_str().contains("serviceToken=test")
                }));
                let mut form = Vec::new();
                request.as_reader().read_to_end(&mut form).unwrap();
                let fields: std::collections::HashMap<_, _> =
                    form_urlencoded::parse(&form).collect();
                let nonce: [u8; 12] = STANDARD
                    .decode(&*fields["_nonce"])
                    .unwrap()
                    .try_into()
                    .unwrap();
                let key: [u8; 32] = Sha256::digest([b"secret".as_slice(), &nonce].concat()).into();
                let raw = decode_cloud(fields["data"].as_bytes(), false, &key).unwrap();
                let input: Value = serde_json::from_slice(&raw).unwrap();
                let response = match request.url() {
                    "/app/v2/home/device_list_page" if input.get("start_did").is_none() => {
                        json!({"list":[{"did":"switch"}],"has_more":true,"next_start_did":"switch"})
                    }
                    "/app/v2/home/device_list_page" => {
                        assert_eq!(input["start_did"], "switch");
                        json!({"list":[{"did":"lamp","name":"灯","spec_type":LAMP_SPEC}]})
                    }
                    "/app/miotspec/prop/get" => {
                        assert_eq!(
                            input,
                            json!({"datasource":1,"params":[{"did":"lamp","siid":7,"piid":9}]})
                        );
                        json!([{"did":"lamp","siid":7,"piid":9,"code":0,"value":on}])
                    }
                    "/app/miotspec/prop/set" => {
                        assert_eq!(
                            input,
                            json!({"params":[{"did":"lamp","siid":7,"piid":9,"value":false}]})
                        );
                        on = false;
                        writes += 1;
                        json!({"params":[{"did":"lamp","siid":7,"piid":9,"code":1}]})
                    }
                    other => panic!("意外的请求路径 {other}"),
                };
                let mut encrypted =
                    serde_json::to_vec(&json!({"code":0,"result":response})).unwrap();
                cipher(&key).apply_keystream(&mut encrypted);
                request
                    .respond(tiny_http::Response::from_string(STANDARD.encode(encrypted)))
                    .unwrap();
            }
            assert_eq!(writes, 1);
        });

        let mut client = Client::new(Session {
            user_id: "1".to_string(),
            c_user_id: String::new(),
            service_token: "test".to_string(),
            ssecurity: SECRET.to_string(),
        });
        client.cloud = format!("{base}/app");
        client.spec = format!("{base}/spec");
        assert!(client.get_power("lamp").unwrap().power);
        assert!(!client.set_power("lamp", false).unwrap().power);
        responder.join().unwrap();
    }
}
