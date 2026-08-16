# rathole-x Linux 服务化计划(systemd)

> 状态：已实现（systemd 后端 `src/platform/systemd.rs`；OpenRC 后端 `src/platform/openrc.rs`；门面 `src/platform/linux.rs` 负责检测 init 系统并分发）。本文档是冻结的设计基准；Windows 侧对应实现见 `src/platform/windows.rs`。

## 目标

`sudo rathole-x service install <server|client>` 在 Linux 上一键完成：写入 init 系统的服务定义、放置角色骨架配置、开机自启并立即启动。支持 systemd 与 OpenRC（Alpine/Gentoo）两个后端，运行时检测自动分发；传统 SysV initd 仍不支持——它没有统一的服务管理命令，碎片化无法抽象（OpenRC 有 rc-update/rc-service 统一入口，可以支持）。

## CLI 语义(与 Windows 对齐)

```
sudo rathole-x service install <server|client> [--name <N>]
sudo rathole-x service uninstall [--name <N>]
```

- 非 root 执行 → 报错 `please run with sudo`(Linux 无 UAC 等价物，不做自提权；这与 Windows 的 `relaunch_elevated_wait` 形成平台差异，有意为之)。
- `service install` 前置条件：配置缺失时自动生成角色骨架配置（`DEFAULT_SERVER_CONFIG`/`DEFAULT_CLIENT_CONFIG`：单角色、零服务、注释引导命令；不做任何网络配置决策）；配置已存在则仅校验可解析。已实现于 `config_edit::ensure_role_config`。
- 已安装服务拒绝 `service install --config <path>`：任意路径无法被 `start`、`stop`、`upgrade` 与 `uninstall --all` 一致发现。请省略 `--config` 并以 `--name <N>` 使用唯一的 `/etc/rathole-x/<N>.toml` 托管配置；前台 `run --config` 仍可使用任意路径。
- 权限模型：
  - `service install` 幂等地创建固定的不可登录系统账号/组 `rathole-x`；若管理员预先创建了该账号则保留原账号，卸载也绝不删除它。
  - `/etc/rathole-x` 为 `root:rathole-x` `0750`；每个 TOML 和 `version.toml` 为 `root:rathole-x` `0640`。root 负责生命周期、配置原子替换和热重载；daemon 仅以组权限读取自己的配置。
  - `service install --allow-user-config` 在 Linux 托管服务中明确拒绝。原子替换必须创建并重命名临时文件，给编辑者父目录写权限会允许其替换或删除其他托管配置，不能以宽松 ACL 规避。
  - `/usr/local/lib/rathole-x/rathole-x` 始终为 root:root `0755`；OpenRC root supervisor 持有 `/run` pid 和 `/var/log` 输出，daemon 不获得这些目录写权限。
  - systemd 只以 `CAP_NET_BIND_SERVICE`（并使用 `CapabilityBoundingSet`、`AmbientCapabilities`、`NoNewPrivileges`）支持低端口；OpenRC 只配置同一 capability。其他网络权限来自普通非 root socket 能力。
  - CLI 侧判定逻辑在 `config_edit::writable_by_current_user` + `platform` 的权限流程。
- 版本契约：多服务模型：`service install server|client --name <N>` 每服务一个配置文件（`/etc/rathole-x/<N>.toml`），服务名 `rathole-x-<role>-<N>`；`version.toml` 由 `service install` 写入 `version = <大版本>`；`config add/set/remove` 在大版本不匹配时拒绝执行（`config_edit::check_version_compat`），修复方式是重装服务；只读命令与 `service install/uninstall` 不受限。**开发规则：任何破坏性配置 schema 变更（改/删字段、改语义或默认值）必须升大版本**；纯新增可选字段（带 serde default）不要求。版本戳放 version.toml 而非主配置，保证主配置文件仍可被上游 rathole 解析。
- 已实现的原则沿用：每个 daemon 或已安装服务进程只承载一个角色。配置同时含 `[server]` 和 `[client]` 时，前台 `run` 必须明确使用 `--server` 或 `--client`；双角色需求使用两个独立配置和进程。

- 自动化契约：`config add/remove/list/set`、`status`、服务生命周期与 `upgrade` 的 `--json` stdout 严格输出一个 `{ok,result}` 或 `{ok:false,error:{message}}` 封套；确认型操作在无人值守或 JSON 情况必须使用 `--yes`，否则非零失败，绝不打印用法后成功退出。
- 运行时状态端点：Windows 当前以规范化配置路径派生的 ACL 本地命名管道提供只读快照；Linux 实现时为每个 unit 提供等价的本地、访问受控端点（不得增加 daemon 网络监听），使 `status` 的 `runtime` 字段一致。端点缺失必须表示 `runtime: null` 与可操作原因，而非让 SCM/配置状态查询失败。

## 文件布局

| 路径 | 说明 |
|---|---|
| `/etc/systemd/system/rathole-x-<role>-<name>.service` | systemd unit（一服务一 unit，单角色） |
| `/etc/init.d/rathole-x-<role>-<name>` | OpenRC openrc-run 脚本（Alpine/Gentoo 等） |
| `/etc/rathole-x/<name>.toml` | 服务配置(install 不覆盖已有文件) |
| 二进制路径 | `/usr/local/lib/rathole-x/rathole-x`；安装和升级时从当前 CLI 原子部署，root:root、`0755`，写进 ExecStart / command= |

## Unit 模板

```ini
[Unit]
Description=rathole-x reverse proxy service (single role)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=rathole-x
Group=rathole-x
ExecStart=/usr/local/lib/rathole-x/rathole-x run --config /etc/rathole-x/<name>.toml
Restart=on-failure
RestartSec=3
LimitNOFILE=1048576
NoNewPrivileges=true
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE
PrivateTmp=true
ProtectHome=true
ProtectSystem=strict

[Install]
WantedBy=multi-user.target
```

说明：
- `Type=simple` + `Restart=on-failure`：复用现有 ctrl-c/broadcast 关停路径，不引入 sd_notify 依赖。
- `LimitNOFILE`：rathole 本就调 `fdlimit::raise_fd_limit()`,unit 层再兜底。
- 配置热更新由现有 `config_watcher`(notify)负责，systemd 不参与 Reload。

## OpenRC 后端（init.d 脚本）

检测到 OpenRC（`/run/openrc` 存在，或安装了 `/sbin/rc-service`；Alpine 3.24 已移除旧 `/sbin/rc` 别名）时使用 openrc-run 脚本：

```sh
#!/sbin/openrc-run
description="rathole-x reverse proxy service (single role)"
command=/usr/local/lib/rathole-x/rathole-x
command_args_foreground="run --config /etc/rathole-x/<name>.toml"
command_user="rathole-x:rathole-x"
capabilities="!cap_chown,...,!cap_net_raw,...,^cap_net_bind_service"
supervisor=supervise-daemon
pidfile="/run/rathole-x-<role>-<name>.pid"
respawn_delay=3
output_log="/var/log/rathole-x-<role>-<name>.log"
error_log="/var/log/rathole-x-<role>-<name>.log"
rc_ulimit="-n 1048576"
depend() { after net firewall; }   # rathole 自带重连，不硬依赖 net
```

- `command_args_foreground` 是 OpenRC 在 `supervisor=supervise-daemon` 时传递 daemon 参数的字段；`command_args` 只用于 start-stop-daemon，不能用于本服务。
- `capabilities` 使用 `supervise-daemon` 的 libcap IAB 格式：显式 block 全部其他 capability，再用 `^cap_net_bind_service` 仅把它作为 ambient capability 传给降权后的 daemon；不能使用 `setcap` 的 `+ep` 文件 capability 格式。
- `supervise-daemon` 等价 `Restart=on-failure`+`RestartSec=3`（respawn_delay）；pidfile 记录的是 supervisor pid。
- enable = `rc-update add <svc> default`；控制 = `rc-service <svc> start|stop|restart`；状态查询 = `rc-service <svc> status` + 读 pidfile（均免 root）。
- 容器里 OpenRC 未 boot 时 install 会补写 `/run/openrc/softlevel` 并调用 `openrc default` 初始化服务状态目录（幂等；真机上是 no-op）。容器 cgroup 警告不会掩盖后续 `rc-service` 的实际安装结果。
- 传统 SysV initd 仍不支持：没有统一入口命令，无法像 rc-update/rc-service 这样抽象。

## 实现（已落地）

1. `src/platform/linux.rs`：门面——`detect_init()`（systemd 优先，OpenRC 具体看上文检测点）分发到后端；承载平台无关部分（root 门禁、version.toml 戳、配置清理、二进制原子替换、uninstall --all、control 目标解析）。
2. `src/platform/systemd.rs`：systemd 后端（本文档上半部分的设计）。
3. `src/platform/openrc.rs`：OpenRC 后端（上一节设计）。
4. systemctl/rc 调用均用 `std::process::Command`，不引入 D-Bus；命令失败原样透传 stderr。
5. 已验收：systemd（Ubuntu 24.04 真机，含开机自启 wants 软链证据与公网 client 端到端）与 OpenRC（Alpine 3.24 容器：install/热加载/stop/start/崩溃 respawn/uninstall 零残留）。容器验收脚本见 `scripts/openrc-container-test.sh`。

## 风险

- WSL/容器无 systemd 且无 OpenRC → `service install` 检测失败则报错说明（明确支持范围）。
- Void(runit)/其他 init → 同上明确不支持。
- Alpine 容器测试需 musl 静态二进制：`cargo zigbuild --release --target x86_64-unknown-linux-musl`（zig 交叉链接，无需 musl-gcc）。
