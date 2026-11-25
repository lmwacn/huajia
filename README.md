# 花甲

<img src="assets/huajia.png" width="200" alt="花甲">

花甲是一个轻量的米家设备控制工具，可在电脑、NAS 和服务器上运行。你可以在终端手动操作，也可以让 AI、脚本和自动化任务通过命令行或 HTTP API 调用。

花甲是个人独立开发的项目，与小米公司无关。

目前支持中国大陆区米家云和兼容 miIO 的 IPv4 局域网设备，可读写 MIoT 灯具、风扇和插座的开关。已导入的设备优先走局域网，本地不可用时自动走米家云。

## 下载与快速开始

从 [GitHub Releases](https://github.com/lmwacn/huajia/releases) 下载对应 Windows、macOS 或 Linux 的 x64 或 ARM64 压缩包。解压后即可运行，无需安装语言运行时。CLI 执行完命令就退出，需要 HTTP 接口时再启动服务。

将可执行文件放入 `PATH` 后运行以下命令。也可以在解压目录里用 `./huajia`（Windows PowerShell 用 `.\huajia.exe`）。

```sh
huajia login
huajia devices --json
huajia get <设备ID> power --json
huajia set <设备ID> power off --json
```

打开 `login` 打印的图片路径，用米家 App 扫码确认。云端授权保存在 `~/.openmihome/session.json`，Windows 上对应 `%USERPROFILE%\.openmihome\session.json`。自动化和 HTTP 服务应以保存授权的用户运行。

## 局域网控制

先用米家云导入一次设备地址、本地 token 和开关属性。CLI 将这些资料保存在 `~/.openmihome/lan/`。设备换 IP 或 token 后，重新导入即可。

```sh
huajia lan import <设备ID>
huajia devices --lan --json
huajia get <设备ID> power --lan --json
huajia set <设备ID> power on --cloud --json
```

- 默认自动选择。对已导入的设备，先检查系统选用的 IPv4 接口与设备地址是否同网段，再用本地协议验证设备身份并读取状态。验证失败时通过米家云执行。
- `--lan` 强制本地。异网、网络隔离、IP 变化或设备不响应时，CLI 会返回错误。蓝牙 Mesh 设备需要有本地转发能力的网关。
- `--cloud` 强制米家云。局域网写入一旦发出，CLI 就不会再经云端重复发出。写入结果不明时，先用 `get` 检查状态。

`devices` 默认列出米家云中的设备，`devices --lan` 列出已导入的本地设备。读写结果的 `transport` 显示实际路径。CLI 的 `--json` 把结果写到 stdout，错误写到 stderr。退出码 `0` 表示成功，`1` 表示执行失败，`2` 表示参数有误。

## HTTP API

```sh
HUAJIA_KEY="$(huajia key create nas)"
huajia serve
# 或指定本机的局域网地址供同网段访问
huajia serve --listen 192.168.1.20:8765
```

默认监听 `127.0.0.1:8765`，`--listen` 接受回环或指定的局域网 IP。HTTP 为明文，跨网络使用时可在前面部署 HTTPS 反向代理。用 `huajia key rotate nas` 获取替换 Key。明文 Key 只在创建或轮换时返回。服务读取本机保存的摘要，轮换无需重启。

在另一个终端设置 `HUAJIA_KEY` 后调用。

```sh
curl -H "Authorization: Bearer $HUAJIA_KEY" \
  "http://127.0.0.1:8765/v1/devices?transport=lan"
curl -H "Authorization: Bearer $HUAJIA_KEY" \
  "http://127.0.0.1:8765/v1/devices/<设备ID>/power"
curl -X PUT -H "Authorization: Bearer $HUAJIA_KEY" \
  -H "Content-Type: application/json" -d '{"power":false}' \
  "http://127.0.0.1:8765/v1/devices/<设备ID>/power?transport=lan"
```

查询参数 `transport` 支持 `auto`、`lan`、`cloud`，含义与 CLI 相同。API Key 可使用这些设备接口。

支持 Shell 的 AI 可直接使用 CLI。项目 Skill 位于 [SKILL.md](.agents/skills/huajia/SKILL.md)。

## 启动时间与资源占用

以下为 `v0.1.0` 发布构建在 Apple M5、macOS ARM64 上的实测结果。

| 指标 | 测量结果 |
| --- | ---: |
| 单文件可执行程序 | 1.85 MiB |
| CLI 启动至 `--help` 执行完毕 | 2.13 ms |
| HTTP 服务从启动到开始监听 | 2.26 ms |
| CLI 执行 `--help` 的峰值常驻内存 | 1.94 MiB |
| HTTP 服务空闲常驻内存 | 2.14 MiB |
| 通过米家云读取真实设备状态的峰值常驻内存 | 11.25 MiB |

CLI 和 HTTP 启动时间分别取 30 次和 10 次预热后运行的中位数。HTTP 内存于监听后空闲 2 秒采样。设备请求的耗时受网络和设备响应影响。

## 构建与测试

```sh
cargo test --locked
cargo build --release --locked
```

产物位于 `target/release/`。花甲采用 [MIT 许可证](LICENSE)。云端通信参考 [mihome-pc-unofficial v0.7.37](https://github.com/shallow-seek-nothing/mihome-pc-unofficial/tree/v0.7.37)，局域网协议参考 [python-miio](https://github.com/rytilahti/python-miio)。
