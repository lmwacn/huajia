mod api;
mod cloud;
mod control;
mod keys;
mod lan;
mod miio;
mod session;
mod update;

use control::Mode;
use std::error::Error;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::process::ExitCode;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "用法
  huajia --version
  huajia update [--check] [--json]
  huajia login
  huajia devices [--lan | --cloud] [--json]
  huajia lan import <设备 ID> [--json]
  huajia get <设备 ID> power [--lan | --cloud] [--json]
  huajia set <设备 ID> power <on|off> [--lan | --cloud] [--json]
  huajia key <create|rotate> <名称> [--json]
  huajia serve [--listen <IP:端口>]

读写默认先尝试已导入的局域网设备。HTTP 服务默认监听 127.0.0.1:8765。
";

enum Command<'a> {
    Login,
    Update(bool),
    Devices(Mode),
    Import(&'a str),
    Power(&'a str, Option<bool>, Mode),
    Key(&'a str, bool),
    Serve(SocketAddr),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] || args == ["-V"] {
        println!("huajia {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let json = args.iter().any(|arg| arg == "--json");
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--json")
        .collect();
    if args.is_empty() || args.contains(&"--help") || args.contains(&"-h") || args[0] == "help" {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let Some(command) = parse(&args, json) else {
        eprintln!("命令或参数无效，运行 huajia --help 查看用法");
        return ExitCode::from(2);
    };
    match execute(command, json, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn parse<'a>(args: &[&'a str], json: bool) -> Option<Command<'a>> {
    let mut mode = Mode::Auto;
    let mut command = Vec::with_capacity(args.len());
    for &arg in args {
        match arg {
            "--lan" if mode == Mode::Auto => mode = Mode::Lan,
            "--cloud" if mode == Mode::Auto => mode = Mode::Cloud,
            "--lan" | "--cloud" => return None,
            _ => command.push(arg),
        }
    }
    match command.as_slice() {
        ["update"] if mode == Mode::Auto => Some(Command::Update(false)),
        ["update", "--check"] if mode == Mode::Auto => Some(Command::Update(true)),
        ["login"] if !json && mode == Mode::Auto => Some(Command::Login),
        ["devices"] => Some(Command::Devices(mode)),
        ["lan", "import", id] if mode == Mode::Auto && !id.is_empty() => Some(Command::Import(id)),
        ["get", id, "power"] if !id.is_empty() => Some(Command::Power(id, None, mode)),
        ["set", id, "power", value @ ("on" | "off")] if !id.is_empty() => {
            Some(Command::Power(id, Some(*value == "on"), mode))
        }
        ["key", action @ ("create" | "rotate"), name] if mode == Mode::Auto => {
            Some(Command::Key(name, *action == "rotate"))
        }
        ["serve"] if !json && mode == Mode::Auto => {
            api::listen_address(api::DEFAULT_LISTEN).map(Command::Serve)
        }
        ["serve", "--listen", address] if !json && mode == Mode::Auto => {
            api::listen_address(address).map(Command::Serve)
        }
        _ => None,
    }
}

fn execute(command: Command<'_>, json: bool, out: &mut impl Write) -> Result<()> {
    match command {
        Command::Update(check_only) => {
            let result = update::run(check_only)?;
            if json {
                return write_json(out, &result);
            }
            match result.status {
                update::Status::UpToDate => {
                    writeln!(out, "当前版本 v{} 已是最新", result.current_version)?
                }
                update::Status::Available => writeln!(
                    out,
                    "发现新版本 v{}，运行 huajia update 安装",
                    result.latest_version
                )?,
                update::Status::Updated => writeln!(
                    out,
                    "花甲已从 v{} 更新到 v{}",
                    result.current_version, result.latest_version
                )?,
            }
        }
        Command::Login => {
            session::login()?.save(&session::path()?)?;
            writeln!(out, "小米授权已保存")?;
        }
        Command::Devices(Mode::Lan) => {
            let devices = lan::list()?;
            if json {
                return write_json(out, &devices);
            }
            writeln!(out, "设备 ID\t名称\t型号\t局域网 IP")?;
            for d in devices {
                writeln!(out, "{}\t{}\t{}\t{}", d.did, d.name, d.model, d.ip)?;
            }
        }
        Command::Devices(_) => {
            let devices = control::cloud()?.devices()?;
            if json {
                return write_json(out, &devices);
            }
            writeln!(out, "设备 ID\t名称\t型号")?;
            for d in devices {
                writeln!(out, "{}\t{}\t{}", d.id, d.name, d.model)?;
            }
        }
        Command::Import(id) => {
            let device = lan::import(&control::cloud()?, id)?;
            if json {
                return write_json(out, &device);
            }
            writeln!(out, "已导入局域网设备 {}（{}）", device.name, device.ip)?;
        }
        Command::Power(id, requested, mode) => {
            let state = control::power(id, requested, mode)?;
            if json {
                return write_json(out, &state);
            }
            let power = if state.power { "on" } else { "off" };
            writeln!(
                out,
                "{}\tpower\t{power}\t{}",
                state.device_id,
                state.transport.name()
            )?;
        }
        Command::Key(name, rotate) => {
            let key = keys::KeyStore::new()?.create(name, rotate)?;
            if json {
                return write_json(out, &key);
            }
            writeln!(out, "{}", key.key)?;
        }
        Command::Serve(addr) => return api::serve(addr),
    }
    Ok(())
}

fn write_json(out: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    writeln!(out)?;
    Ok(())
}
