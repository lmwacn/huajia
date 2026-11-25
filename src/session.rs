use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use ureq::http::{HeaderMap, header};
use url::Url;

const ACCOUNT: &str = "https://account.xiaomi.com";
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize, Serialize)]
pub struct Session {
    pub user_id: String,
    pub c_user_id: String,
    pub service_token: String,
    pub ssecurity: String,
}

pub fn path() -> Result<PathBuf> {
    Ok(directory()?.join("session.json"))
}

pub fn directory() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or("无法确定当前用户的主目录")?
        .join(".openmihome"))
}

impl Session {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => "尚未登录，请运行 huajia login".into(),
            _ => Box::<dyn std::error::Error>::from(err),
        })?;
        let session: Self =
            serde_json::from_reader(file).map_err(|_| "授权文件无效，请重新登录")?;
        if !session.valid() {
            return Err("授权文件无效，请重新登录".into());
        }
        Ok(session)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        save_private_json(path, self)
    }

    fn valid(&self) -> bool {
        !(self.user_id.is_empty() && self.c_user_id.is_empty()
            || self.service_token.is_empty()
            || self.ssecurity.is_empty())
    }
}

pub fn save_private_json(path: &Path, data: &impl Serialize) -> Result<()> {
    write_private_json(path, data, false)
}

pub fn save_new_private_json(path: &Path, data: &impl Serialize) -> Result<()> {
    write_private_json(path, data, true)
}

fn write_private_json(path: &Path, data: &impl Serialize, create: bool) -> Result<()> {
    let dir = path.parent().ok_or("本地配置路径无效")?;
    create_config_directory(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(tmp.as_file_mut(), data)?;
    tmp.as_file_mut().flush()?;
    if create {
        tmp.persist_noclobber(path)?;
    } else {
        tmp.persist(path)?;
    }
    Ok(())
}

fn create_config_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.recursive(true).create(path)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Reply {
    code: Option<i32>,
    qr: String,
    lp: String,
    timeout: u64,
    location: String,
    #[serde(rename = "userId")]
    user_id: Value,
    #[serde(rename = "cUserId")]
    c_user_id: String,
    ssecurity: String,
    #[serde(rename = "serviceToken")]
    service_token: String,
}

pub fn login() -> Result<Session> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .build()
        .into();
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).map_err(|err| format!("无法生成登录会话 ID: {err}"))?;
    let cookie = format!("deviceId={:016x}; uLocale=zh_CN", u64::from_be_bytes(id));

    let mut start = Url::parse(ACCOUNT)?.join("/longPolling/loginUrl")?;
    start
        .query_pairs_mut()
        .append_pair("sid", "xiaomiio")
        .append_pair("_qrsize", "360")
        .append_pair(
            "_dc",
            &SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_millis()
                .to_string(),
        );
    let (raw, _) = get(&agent, start, &cookie, TIMEOUT)?;
    let challenge = parse_reply(&raw)?;
    check_code(challenge.code)?;
    let qr = login_url(&challenge.qr)?;
    let poll = login_url(&challenge.lp)?;
    let (image, _) = get(&agent, qr, &cookie, TIMEOUT)?;
    if !image.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("小米返回的二维码不是 PNG 图片".into());
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("huajia-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir()?;
    let image_path = dir.path().join("login.png");
    fs::write(&image_path, image)?;
    eprintln!(
        "用米家 App 扫描此图片并在手机上确认\n{}",
        image_path.display()
    );

    let timeout = if (1..=300).contains(&challenge.timeout) {
        challenge.timeout
    } else {
        300
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("扫码等待已超时".into());
        }
        match get(
            &agent,
            poll.clone(),
            &cookie,
            remaining.min(Duration::from_secs(60)),
        ) {
            Ok((raw, headers)) => {
                let reply = parse_reply(&raw)?;
                match reply.code {
                    Some(0) => return exchange(&agent, reply, &headers, &cookie),
                    Some(70016..=70021) => {}
                    code => check_code(code)?,
                }
            }
            Err(err)
                if matches!(
                    err.downcast_ref::<ureq::Error>(),
                    Some(ureq::Error::Timeout(_))
                ) => {}
            Err(err) => return Err(err),
        }
        thread::sleep(
            Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn exchange(
    agent: &ureq::Agent,
    mut reply: Reply,
    headers: &HeaderMap,
    cookie: &str,
) -> Result<Session> {
    if reply.ssecurity.is_empty()
        && let Ok(extension) =
            serde_json::from_str::<Reply>(header_str(headers, "extension-pragma"))
    {
        reply.ssecurity = extension.ssecurity;
    }
    if reply.c_user_id.is_empty() {
        reply.c_user_id = header_str(headers, "cuserid").to_string();
    }
    let location = if reply.location.is_empty() {
        header_str(headers, "location")
    } else {
        &reply.location
    };
    let (raw, received) = get(agent, login_url(location)?, cookie, TIMEOUT)?;
    let service_token = received
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|value| {
            value
                .to_str()
                .ok()?
                .split(';')
                .next()?
                .trim()
                .strip_prefix("serviceToken=")
        });
    let service_token = match service_token.filter(|value| !value.is_empty()) {
        Some(token) => token.to_string(),
        None => parse_reply(&raw)
            .map(|reply| reply.service_token)
            .unwrap_or_default(),
    };
    let user_id = match reply.user_id {
        Value::String(id) => id,
        Value::Number(id) => id.to_string(),
        _ => String::new(),
    };
    let session = Session {
        user_id,
        c_user_id: reply.c_user_id,
        service_token,
        ssecurity: reply.ssecurity,
    };
    if !session.valid() {
        return Err("小米登录未返回完整的服务授权".into());
    }
    Ok(session)
}

fn login_url(raw: &str) -> Result<Url> {
    if raw.is_empty() {
        return Err("小米登录返回了无效的地址".into());
    }
    let target = Url::parse(ACCOUNT)?.join(raw)?;
    trusted(&target)?;
    Ok(target)
}

fn trusted(url: &Url) -> Result<()> {
    let Some(host) = url.host_str() else {
        return Err("小米登录返回了无效的地址".into());
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !(matches!(
            host,
            "account.xiaomi.com" | "sts.api.io.mi.com" | "api.io.mi.com" | "account.mi.com"
        ) || host.ends_with(".account.xiaomi.com"))
    {
        return Err("小米登录返回了无效的地址".into());
    }
    Ok(())
}

fn get(
    agent: &ureq::Agent,
    mut target: Url,
    cookie: &str,
    timeout: Duration,
) -> Result<(Vec<u8>, HeaderMap)> {
    let deadline = Instant::now() + timeout;
    let mut cookies = HeaderMap::new();
    for _ in 0..8 {
        trusted(&target)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut response = agent
            .get(target.as_str())
            .header(header::COOKIE, cookie)
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()
            .map_err(|err| match err {
                ureq::Error::Timeout(_) => Box::<dyn std::error::Error>::from(err),
                _ => "无法连接小米登录服务".into(),
            })?;
        for value in response.headers().get_all(header::SET_COOKIE) {
            cookies.append(header::SET_COOKIE, value.clone());
        }
        if response.status().is_redirection() {
            target = target
                .join(header_str(response.headers(), "location"))
                .map_err(|_| "小米登录重定向地址无效")?;
            continue;
        }
        let mut headers = response.headers().clone();
        headers.remove(header::SET_COOKIE);
        headers.extend(cookies);
        let body = response
            .body_mut()
            .with_config()
            .limit(2 << 20)
            .read_to_vec()?;
        return Ok((body, headers));
    }
    Err("小米登录重定向次数过多".into())
}

fn header_str<'a>(headers: &'a HeaderMap, key: &str) -> &'a str {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn parse_reply(raw: &[u8]) -> Result<Reply> {
    let raw = raw.trim_ascii_start();
    let raw = raw.strip_prefix(b"&&&START&&&").unwrap_or(raw);
    serde_json::from_slice(raw).map_err(|_| "小米登录返回了无法识别的响应".into())
}

fn check_code(code: Option<i32>) -> Result<()> {
    match code {
        Some(0) => Ok(()),
        Some(code) => Err(format!("小米登录失败，状态码 {code}").into()),
        None => Err("小米登录响应缺少状态码".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config").join("session.json");
        let mut session = Session {
            user_id: "1".into(),
            c_user_id: String::new(),
            service_token: "test-first".into(),
            ssecurity: "test-secret".into(),
        };
        session.save(&path).unwrap();
        assert_eq!(Session::load(&path).unwrap().service_token, "test-first");
        session.service_token = "test-second".into();
        session.save(&path).unwrap();
        assert_eq!(Session::load(&path).unwrap().service_token, "test-second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn login_targets_are_restricted() {
        assert!(login_url("/longPolling/loginUrl").is_ok());
        assert!(login_url("https://c3.lp.account.xiaomi.com/scan").is_ok());
        assert!(login_url("https://sts.api.io.mi.com/sts").is_ok());
        assert!(login_url("https://account.xiaomi.com.example/scan").is_err());
        assert!(login_url("http://account.xiaomi.com/scan").is_err());
    }
}
