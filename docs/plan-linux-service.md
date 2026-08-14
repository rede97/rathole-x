# rathole-x Linux 服务化计划(systemd)

> 状态：计划文档，未实现。当前阶段优先 Windows 服务实现；本文件冻结 Linux 侧设计，实现时按此执行。
> Windows 侧对应实现见 `src/windows_service.rs`。

## 目标

`sudo rathole-x install` 在 Linux 上一键完成：写入 systemd unit、放置默认配置、开机自启并立即启动。initd(SysV)明确不支持——发行版碎片化严重，现代发行版均为 systemd。

## CLI 语义(与 Windows 对齐)

```
sudo rathole-x install [-c /etc/rathole-x.toml] [--name rathole-x]
sudo rathole-x uninstall [--name rathole-x]
```

- 非 root 执行 → 报错 `please run with sudo`(Linux 无 UAC 等价物，不做自提权；这与 Windows 的 `relaunch_elevated_and_exit` 形成平台差异，有意为之)。
- `install` 前置条件：配置缺失时自动生成统一空配置（`DEFAULT_CONFIG`：静默 client、零服务、server 段注释引导；不做任何网络配置决策）；配置已存在则仅校验可解析。已实现于 `config_edit::ensure_install_config`。
- 权限模型（已实现于 Windows 侧，Linux 实现期沿用）：
  - `version.toml`（与配置同目录）记录 `version = <大版本>`，由 `install` 写入；无权限策略文件——CLI 以文件实际写权限（探测）决定是否提权，`install --allow-user-config` 仅设置 ACL。
  - 当前用户对配置可写（ACL 授权）：CLI 免提权直接原子写配置（临时文件 + rename），服务经 watcher 热加载。
  - 不可写（默认）：CLI 须提权（Windows 走 UAC `relaunch_elevated_wait`；Linux 对应 sudo）后覆盖配置，服务热加载。
  - CLI 侧判定逻辑在 `config_edit::writable_by_current_user`（打开探测/目录探测）+ `platform` 提权流程。
- 版本契约：多服务模型：`service install server|client --name <N>` 每服务一个配置文件（`/etc/rathole-x/<N>.toml`），SCM 名 `rathole-x-<role>-<N>`；`version.toml` 由 install 写入 `version = <大版本>`；`config add/set/remove` 在大版本不匹配时拒绝执行（`config_edit::check_version_compat`），修复方式是重装服务；只读命令与 install/uninstall 不受限。**开发规则：任何破坏性配置 schema 变更（改/删字段、改语义或默认值）必须升大版本**；纯新增可选字段（带 serde default）不要求。版本戳放 version.toml 而非主配置，保证主配置文件仍可被上游 rathole 解析。
- 已实现的原则沿用：daemon 单进程可同时承载 [server] 与 [client] 双模式(lib.rs `RunMode::Both`)。

## 文件布局

| 路径 | 说明 |
|---|---|
| `/etc/systemd/system/<name>.service` | unit 文件 |
| `/etc/rathole-x.toml` | 默认配置(install 不覆盖已有文件) |
| 二进制路径 | `std::env::current_exe()`,写进 ExecStart |

## Unit 模板

```ini
[Unit]
Description=rathole-x reverse proxy (server+client)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=<current_exe> run --config /etc/rathole-x.toml
Restart=on-failure
RestartSec=3
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
```

说明：
- `Type=simple` + `Restart=on-failure`：复用现有 ctrl-c/broadcast 关停路径，不引入 sd_notify 依赖。
- `LimitNOFILE`：rathole 本就调 `fdlimit::raise_fd_limit()`,unit 层再兜底。
- 配置热更新由现有 `config_watcher`(notify)负责，systemd 不参与 Reload。

## 实现步骤(实现期)

1. 新建 `src/install_linux.rs`,`#[cfg(target_os = "linux")]`,API 与 Windows 侧对称：
   `install(SystemdInstallOptions) -> Result<()>` / `uninstall(service_name) -> Result<()>`。
2. lib.rs 的 `install/uninstall` 分发：`cfg(linux)` 分支替换当前的 "planned" 提示。
3. systemctl 调用用 `std::process::Command`(`daemon-reload`、`enable --now`),不引入 D-Bus 依赖;命令失败原样透传 stderr。
4. `uninstall`:`disable --now`(best-effort)→ 删 unit → `daemon-reload`。
5. 验收：在 Linux 机器或容器实测 `install` 后 `systemctl status` active、重启后自启、`uninstall` 无残留。

## 风险

- WSL/容器无 systemd → `install` 检测 `/run/systemd/system` 不存在则报错说明。
- 非 systemd 发行版(Alpine/Void)→ 同一检测自然报错，不支持即明确不支持。
