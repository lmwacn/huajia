use crate::Result;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::time::Duration;

const REPOSITORY: &str = "https://github.com/lmwacn/huajia";
const LATEST_API: &str = "https://api.github.com/repos/lmwacn/huajia/releases/latest";
const RELEASE_LIMIT: u64 = 1 << 20;
const ARCHIVE_LIMIT: u64 = 32 << 20;
const PROGRAM_LIMIT: u64 = 16 << 20;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    digest: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    UpToDate,
    Available,
    Updated,
}

#[derive(Serialize)]
pub struct Update {
    pub status: Status,
    pub current_version: String,
    pub latest_version: String,
}

pub fn run(check_only: bool) -> Result<Update> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(5)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
    let release: Release = serde_json::from_slice(&fetch(
        &agent,
        LATEST_API,
        "application/vnd.github+json",
        RELEASE_LIMIT,
    )?)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let latest = release_version(&release.tag_name)?;
    let mut result = Update {
        status: Status::UpToDate,
        current_version: current.to_string(),
        latest_version: latest.to_string(),
    };
    if latest.cmp_precedence(&current) != Ordering::Greater {
        return Ok(result);
    }
    if check_only {
        result.status = Status::Available;
        return Ok(result);
    }

    let asset_name =
        asset_name(std::env::consts::OS, std::env::consts::ARCH).ok_or("当前平台没有官方更新包")?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or("GitHub Release 缺少当前平台的更新包")?;
    let digest = asset
        .digest
        .as_deref()
        .ok_or("GitHub 未提供更新包的 SHA-256")?;

    let executable = std::env::current_exe()?.canonicalize()?;
    let directory = executable.parent().ok_or("无法确定 huajia 安装目录")?;
    // 锁与安装目录绑定，避免多个更新进程同时替换同一份程序。
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".huajia-update.lock"))
        .map_err(|err| format!("无法写入 huajia 安装目录: {err}"))?;
    lock.try_lock()
        .map_err(|err| format!("无法取得更新锁: {err}"))?;

    let url = format!("{REPOSITORY}/releases/download/v{latest}/{asset_name}");
    let archive = fetch(&agent, &url, "application/octet-stream", ARCHIVE_LIMIT)?;
    verify_digest(&archive, digest)?;
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    extract_program(&archive, staged.as_file_mut())?;
    staged.as_file().sync_all()?;
    install_program(staged, &executable)
        .map_err(|err| format!("无法替换已安装的 huajia: {err}"))?;

    result.status = Status::Updated;
    Ok(result)
}

#[cfg(unix)]
fn install_program(staged: tempfile::NamedTempFile, executable: &Path) -> Result<()> {
    staged
        .as_file()
        .set_permissions(executable.metadata()?.permissions())?;
    staged.persist(executable)?;
    Ok(())
}

#[cfg(windows)]
fn install_program(staged: tempfile::NamedTempFile, executable: &Path) -> Result<()> {
    let directory = executable.parent().ok_or("无法确定 huajia 安装目录")?;
    // Windows 替换过程会先移开运行中的文件，因此保留可用于回滚的副本。
    let mut rollback = tempfile::NamedTempFile::new_in(directory)?;
    io::copy(&mut File::open(executable)?, rollback.as_file_mut())?;
    rollback.as_file().sync_all()?;
    if let Err(err) = self_replace::self_replace(staged.path()) {
        if !executable.exists()
            && let Err(failure) = rollback.persist(executable)
        {
            let (_, backup) = failure.file.keep()?;
            return Err(format!("{err}；旧版本备份位于 {}", backup.display()).into());
        }
        return Err(err.into());
    }
    Ok(())
}

fn release_version(tag: &str) -> Result<Version> {
    let version = Version::parse(tag.strip_prefix('v').ok_or("GitHub 版本标签缺少 v 前缀")?)?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err("GitHub 更新需使用 vX.Y.Z 格式的正式版本标签".into());
    }
    Ok(version)
}

fn asset_name(os: &str, arch: &str) -> Option<String> {
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    let suffix = match os {
        "macos" | "linux" => "tar.gz",
        "windows" => "zip",
        _ => return None,
    };
    Some(format!("huajia-{os}-{arch}.{suffix}"))
}

fn fetch(agent: &ureq::Agent, url: &str, accept: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = agent
        .get(url)
        .header("User-Agent", concat!("huajia/", env!("CARGO_PKG_VERSION")))
        .header("Accept", accept)
        .call()
        .map_err(|err| format!("无法从 GitHub 取得更新: {err}"))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()?;
    Ok(bytes)
}

fn verify_digest(data: &[u8], digest: &str) -> Result<()> {
    let actual = format!("sha256:{:x}", Sha256::digest(data));
    if !actual.eq_ignore_ascii_case(digest) {
        return Err("更新包的 SHA-256 校验失败".into());
    }
    Ok(())
}

#[cfg(unix)]
fn extract_program(archive: &[u8], out: &mut File) -> Result<()> {
    let gzip = flate2::read::GzDecoder::new(Cursor::new(archive));
    let mut tar = tar::Archive::new(gzip.take(ARCHIVE_LIMIT));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() == std::path::Path::new("huajia")
            && entry.header().entry_type().is_file()
        {
            return copy_program(&mut entry, out);
        }
    }
    Err("更新包中缺少 huajia 可执行文件".into())
}

#[cfg(windows)]
fn extract_program(archive: &[u8], out: &mut File) -> Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive))?;
    let mut entry = zip.by_name("huajia.exe")?;
    if !entry.is_file() || entry.is_symlink() {
        return Err("更新包中缺少 huajia.exe 可执行文件".into());
    }
    copy_program(&mut entry, out)
}

fn copy_program(input: &mut impl Read, output: &mut File) -> Result<()> {
    let size = io::copy(&mut input.take(PROGRAM_LIMIT + 1), output)?;
    if !(1..=PROGRAM_LIMIT).contains(&size) {
        return Err("更新包中的可执行文件大小无效".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_published_asset_names_and_versions() {
        for (os, arch, expected) in [
            ("macos", "aarch64", "huajia-macos-arm64.tar.gz"),
            ("linux", "x86_64", "huajia-linux-x64.tar.gz"),
            ("windows", "aarch64", "huajia-windows-arm64.zip"),
        ] {
            assert_eq!(asset_name(os, arch).as_deref(), Some(expected));
        }
        assert!(asset_name("linux", "arm").is_none());
        assert!(release_version("v0.1.10").unwrap() > release_version("v0.1.9").unwrap());
        assert!(release_version("v0.2.0-beta.1").is_err());
        assert!(release_version("v../0.2.0").is_err());
    }

    #[test]
    fn rejects_modified_update_archive() {
        let digest = format!("sha256:{:x}", Sha256::digest(b"verified"));
        assert!(verify_digest(b"verified", &digest).is_ok());
        assert!(verify_digest(b"modified", &digest).is_err());
        assert!(verify_digest(b"verified", "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stages_only_the_expected_regular_binary() {
        use std::io::{Seek, SeekFrom};

        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut tar = tar::Builder::new(&mut gzip);
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            header.set_size(3);
            header.set_cksum();
            tar.append_data(&mut header, "huajia", &b"bin"[..]).unwrap();
            tar.finish().unwrap();
        }
        let mut out = tempfile::tempfile().unwrap();
        extract_program(&gzip.finish().unwrap(), &mut out).unwrap();
        out.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = Vec::new();
        out.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"bin");
    }
}
