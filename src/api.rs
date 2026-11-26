use crate::{
    Result,
    control::{self, Mode},
    home,
    keys::KeyStore,
    miio,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use tiny_http::{Header, Method, Request, Response, Server};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8765";
const MAX_BODY: u64 = 4096;

pub fn listen_address(value: &str) -> Option<SocketAddr> {
    let addr: SocketAddr = value.parse().ok()?;
    let allowed = match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback() || miio::local_ip(ip),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
    };
    allowed.then_some(addr)
}

pub fn serve(addr: SocketAddr) -> Result<()> {
    let keys = KeyStore::new()?;
    let server = Server::http(addr).map_err(|err| format!("无法启动 HTTP 服务: {err}"))?;
    eprintln!("花甲 HTTP 正在监听 http://{}", server.server_addr());
    for mut request in server.incoming_requests() {
        let (status, body) = handle(&mut request, &keys);
        let mut response = Response::from_string(body.to_string()).with_status_code(status);
        for (name, value) in [
            ("Content-Type", "application/json"),
            ("Cache-Control", "no-store"),
        ] {
            response.add_header(Header::from_bytes(name, value).expect("固定 HTTP 响应头"));
        }
        if status == 401 {
            response.add_header(
                Header::from_bytes("WWW-Authenticate", "Bearer").expect("固定鉴权响应头"),
            );
        }
        if let Err(err) = request.respond(response) {
            eprintln!("HTTP 响应发送失败: {err}");
        }
    }
    Ok(())
}

fn handle(request: &mut Request, keys: &KeyStore) -> (u16, Value) {
    let auth = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"));
    if !auth.is_some_and(|header| keys.authorized(header.value.as_str())) {
        return (401, json!({"error": "缺少有效的 Bearer Key"}));
    }

    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let Some((mode, home_id)) = device_query(query) else {
        return (
            400,
            json!({"error": "查询参数无效，transport 取值须为 auto、lan 或 cloud"}),
        );
    };
    if home_id.is_some() && path != "/v1/devices" {
        return (400, json!({"error": "home 参数只用于查询设备列表"}));
    }
    let result = match (request.method(), path) {
        (&Method::Get, "/v1/homes") if query.is_empty() => home::list().and_then(to_json),
        (&Method::Get, "/v1/devices") if mode == Mode::Lan => {
            control::lan_devices(home_id.as_deref()).and_then(to_json)
        }
        (&Method::Get, "/v1/devices") => {
            control::cloud_devices(home_id.as_deref()).and_then(to_json)
        }
        (method @ (&Method::Get | &Method::Put), path) => {
            let Some(id) = path
                .strip_prefix("/v1/devices/")
                .and_then(|id| id.strip_suffix("/power"))
                .filter(|id| {
                    !id.is_empty()
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
                })
            else {
                return (404, json!({"error":"接口不存在"}));
            };
            let operation = if method == &Method::Put {
                let Some(power) = parse_body(request) else {
                    return (
                        400,
                        json!({"error":"请求体须为包含布尔值 power 的 JSON 对象"}),
                    );
                };
                Some(power)
            } else {
                None
            };
            control::power(id, operation, mode).and_then(to_json)
        }
        _ => return (404, json!({"error":"接口不存在"})),
    };
    match result {
        Ok(body) => (200, body),
        Err(err) => (502, json!({"error":err.to_string()})),
    }
}

fn device_query(query: &str) -> Option<(Mode, Option<Cow<'_, str>>)> {
    let mut mode = None;
    let mut home = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "transport" if mode.is_none() => {
                mode = Some(match value.as_ref() {
                    "auto" => Mode::Auto,
                    "lan" => Mode::Lan,
                    "cloud" => Mode::Cloud,
                    _ => return None,
                });
            }
            "home" if home.is_none() && !value.is_empty() => home = Some(value),
            _ => return None,
        }
    }
    Some((mode.unwrap_or(Mode::Auto), home))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PowerBody {
    power: bool,
}

fn parse_body(request: &mut Request) -> Option<bool> {
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .ok()?;
    if body.len() as u64 > MAX_BODY {
        return None;
    }
    serde_json::from_slice::<PowerBody>(&body)
        .ok()
        .map(|body| body.power)
}

fn to_json(data: impl serde::Serialize) -> Result<Value> {
    Ok(serde_json::to_value(data)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_requires_an_explicit_local_address() {
        assert!(listen_address(DEFAULT_LISTEN).is_some());
        assert!(listen_address("192.168.1.20:8765").is_some());
        assert!(listen_address("[::1]:8765").is_some());
        assert!(listen_address("0.0.0.0:8765").is_none());
        assert!(listen_address("8.8.8.8:8765").is_none());
    }

    #[test]
    fn inventory_query_accepts_a_single_home_and_transport() {
        let (mode, home) = device_query("home=123&transport=lan").unwrap();
        assert!(mode == Mode::Lan);
        assert_eq!(home.as_deref(), Some("123"));
        assert!(device_query("home=123&home=456").is_none());
        assert!(device_query("home=").is_none());
        assert!(device_query("transport=lan&transport=cloud").is_none());
    }
}
