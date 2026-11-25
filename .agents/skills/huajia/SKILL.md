---
name: huajia
description: 使用 花甲 CLI 列出米家设备、读取开关状态或通过局域网和米家云执行已授权的开关操作。
---

优先用 `huajia devices --json` 获取设备 ID。离线时用 `huajia devices --lan --json` 查看已导入的设备。若有同名设备，用型号或设备 ID 向用户确认目标。

- `huajia get <设备 ID> power --json` 读取布尔值 `power`。添加 `--lan` 或 `--cloud` 可强制指定路径。省略时自动选择，输出的 `transport` 显示实际路径。
- `huajia set <设备 ID> power <on|off> --json` 的成功结果包含操作后读回的状态。只有用户授权后才执行。写入未获确认时，先查状态，避免换路径重复写入。
- 首次使用局域网控制时，运行 `huajia lan import <设备 ID>`。若尚未进行米家授权，请用户用 `huajia login` 扫码。本地授权由 CLI 读取，终端命令无需传入设备 token。
