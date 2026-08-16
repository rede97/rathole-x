# rathole

![rathole-logo](./docs/img/rathole-logo.png)

[![GitHub stars](https://img.shields.io/github/stars/rapiz1/rathole)](https://github.com/rapiz1/rathole/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/rapiz1/rathole)](https://github.com/rapiz1/rathole/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/rapiz1/rathole/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/rapiz1/rathole/total)](https://github.com/rapiz1/rathole/releases)
[![Docker Pulls](https://img.shields.io/docker/pulls/rapiz1/rathole)](https://hub.docker.com/r/rapiz1/rathole)

[English](README.md) | [简体中文](README-zh.md)

安全、稳定、高性能的内网穿透工具，用 Rust 语言编写

rathole，类似于 [frp](https://github.com/fatedier/frp) 和 [ngrok](https://github.com/inconshreveable/ngrok)，可以让 NAT 后的设备上的服务通过具有公网 IP 的服务器暴露在公网上。

# rathole-x (fork)

`rathole-x` 是 [rathole](https://github.com/rapiz1/rathole) 的增强分支，保持**线协议与上游 100% 兼容**——上游 rathole 的客户端与服务端均可与 `rathole-x` 互通——同时新增了子命令驱动的 CLI、免手写配置文件的流程，以及更深入的平台集成。二进制文件名为 `rathole-x`。

## Design philosophy

- **单一二进制、子命令驱动的 CLI。** 对人友好：交互式提示 + 自动生成的 token 与 noise 密钥。对脚本/Agent 友好：所有操作都有对应 flag，支持 `--json` 输出与 `--yes` 非交互模式。
- **免手写配置文件。** `service install <server|client>` 部署系统服务并创建角色专属的骨架配置；`config add`/`config set` 管理全部内容；热重载无需重启即可应用变更。
- **一个进程一个角色。** 每个前台或已安装进程只运行 server 或 client 之一，并使用角色专属配置；同一主机需要两种角色时配置并运行两个独立进程。
- **与上游兼容的线协议。** `rathole-x` 使用与上游 rathole 相同的线协议，因此二者可互通。
- **平台集成。** Windows SCM 服务 + UAC 提权；Linux systemd 已规划（见 [docs/plan-linux-service.md](docs/plan-linux-service.md)）。
- **安全默认值。** token 为必填；配置编辑受实际文件权限约束——CLI 会探测当前用户是否可写该配置，仅当不可写时才提权（UAC）；服务二进制被复制到 `ProgramData`，非管理员无法替换。只读状态（SCM 状态、runtime 连接快照、已安装配置树）对本地用户可读、无需 UAC；`permission denied`、`missing`、`unavailable` 三类原因分别明确报告。

## New features over upstream

- **子命令驱动的 CLI** — `run`（运行守护进程）、`config add|remove|list|set`（管理服务配置）、`status`（服务状态 + 配置树）、`genkey`（生成 noise 密钥对）、`service install|uninstall|start|stop|restart`（系统服务生命周期）、`upgrade`（更新已安装二进制）。各 flag 见 [CLI reference](#cli-reference)。
- **单角色执行** — 同时含 `[server]` 与 `[client]` 的手写配置必须显式使用 `run --server` 或 `run --client`；常规部署使用两个独立配置的进程。
- **自动生成 token 与 noise 密钥** — `config add`/`config set` 在省略时自动生成。
- **原子写入热重载** — `config add`/`config set`/`config remove` 原子重写配置；运行中的服务无需重启即热重载。
- **Windows 服务安装** — `service install server|client --name <n>` 注册命名的 AutoStart SCM 服务，运行身份是 `NT AUTHORITY\LocalService`，带**受限的每服务 SID**，绝不使用 LocalSystem。共享受保护二进制仅向已安装的 `NT SERVICE\rathole-x-<role>-<n>` SID 授予读/执行；每个服务有独立受保护的日志目录；已安装配置对本地用户只读，因此只读 `status` 无需 UAC，默认仅 Administrators 可编辑（`--allow-user-config` 显式授予 Users 写权限）。
- **Linux systemd / OpenRC 服务安装** — 同一 `service install server|client --name <n>` 会在运行时检测 init 系统：systemd 写入开机启用的受限非 root unit；OpenRC（Alpine、Gentoo）生成以 `command_args_foreground` 让守护进程作为 `rathole-x` 运行的 openrc-run 脚本，root 仅保留 supervisor/pid/log 生命周期职责。低端口只通过 `CAP_NET_BIND_SERVICE` 支持。
- **角色骨架自动创建** — `service install` 在缺失时创建角色专属的骨架配置。

## Quick start (Windows)

1. 从已提权的 shell 安装服务。安装前会弹出交互式确认（传入 `--yes` 可跳过；非交互或 `--json` 调用不传 `--yes` 时返回可操作错误并以非零退出）。若缺少配置文件则自动创建角色专属的骨架配置，二进制被复制到配置旁，并以 UAC 提权注册一个 Windows SCM 服务（AutoStart）：

```bash
# 服务端（公网 IP）
./rathole-x service install server --yes --name relay

# 客户端（NAT 之后）
./rathole-x service install client --yes --name home-nas
```

2. 添加服务。以下示例暴露 NAS 的 ssh 服务：同一条目名将 server 与 client 两侧配对，token 自动生成：

```bash
# 服务端（公网 IP）：将 5202 端口暴露到互联网
./rathole-x config add --name relay --server "name:my_nas_ssh;bind:0.0.0.0:5202"

# 客户端（NAT 之后）：先指向服务器（一次），再转发到 NAS 上 22 端口的 ssh 守护进程
./rathole-x config set --name home-nas --client --remote-addr myserver.com:2333
./rathole-x config add --name home-nas --client "name:my_nas_ssh;local:127.0.0.1:22"
```

3. 查看服务状态与配置树：

```bash
./rathole-x status
```

4. 卸载服务。最后一个服务被移除时 `version.toml` 一并删除；除非传入 `--purge`，否则配置文件保留：

```bash
./rathole-x service uninstall --yes --name relay
./rathole-x service uninstall --yes --name home-nas
```

> **Linux systemd / OpenRC**：同样的命令须以 `sudo` 运行（`sudo rathole-x service install server --yes --name relay`）。systemd 写入、启用并启动 `rathole-x-server-relay.service`；OpenRC（Alpine、Gentoo）写入 `/etc/init.d/rathole-x-server-relay`，并通过 `rc-update add default` 注册。两者都以专用、不可登录的 `rathole-x:rathole-x` 账号而不是 root 运行受保护的 root:root `0755` `/usr/local/lib/rathole-x/rathole-x`。低端口只通过 `CAP_NET_BIND_SERVICE` 支持；Linux 不做自提权，非 root 运行会报 `please run with sudo`。见 [docs/plan-linux-service.md](docs/plan-linux-service.md)。

## Service lifecycle

- `rathole-x service start|stop|restart [--name N | --all]` — 驱动已安装服务的 SCM 状态（需要时 UAC 提权）。
- `rathole-x upgrade --yes` — 原地更新已安装的二进制：停止所有服务、用当前运行的二进制替换共享二进制、再重新启动它们。
- `rathole-x service uninstall --yes --all` — 移除所有已安装服务与共享二进制，但保留配置；加 `--purge` 才同时删除全部配置。
- 常规的 `service uninstall --yes` 会保留可被用户删除的配置；而卸载一个已移除的服务会在**不提权**的情况下清理残留文件。

## Config schema versioning

`version.toml` 文件（由 `service install` 写入）记录了安装该服务的 rathole-x 构建的**大版本号**。`config add`/`config set`/`config remove` 会拒绝操作其大版本戳与当前 CLI 不一致的配置：

```text
This config is managed by rathole-x v0 but this CLI is v1.
Reinstall the service to upgrade: `rathole-x service uninstall --yes` then `rathole-x service install <server|client> --yes`.
```

只读命令（`status`、`config list`、`run`）以及 `install`/`uninstall` 不被版本戳拦截。没有版本戳的配置（用户自行管理的文件）无限制。

**开发者规则：任何破坏性的配置 schema 变更（重命名/删除/重新打标签的字段、语义或默认值变更）都必须升级大版本号。** 纯增量变更（带 serde 默认值的新可选字段）则无需升级。版本戳存放在 `version.toml` 中，因此 rathole 配置文件本身仍可被上游 rathole 解析。

## Docker

在 Docker 宿主机上完全不需要 init 系统：Docker 本身就是生命周期管理器（`restart` = 崩溃重启，Docker 守护进程 = 开机自启，`docker stop` 发送 SIGTERM）。镜像**不会启动或调用 systemd/OpenRC**。入口点只做挂载配置目录所需的最小所有权初始化，随后以非 root 的 `rathole-x` 身份 `exec` 前台 `rathole-x run`；Docker 管理该进程，守护进程热加载挂载配置。

```bash
docker build -t rathole-x .
docker run -d --name rathole-x -v rathole-x-conf:/etc/rathole-x --restart unless-stopped rathole-x
# 容器先空配置启动并休眠等待。以下命令原子创建挂载配置，不需要
# 手写 TOML 或 init 系统：
docker exec rathole-x rathole-x config set -c /etc/rathole-x/rathole-x.toml --client --remote-addr myserver.com:2333
docker exec rathole-x rathole-x config add -c /etc/rathole-x/rathole-x.toml myssh --local-addr 172.17.0.1:22
```

现成的 [docker-compose.yml](examples/docker/docker-compose.yml) 在 `examples/docker/`。client 模式通常需要 `network_mode: host`（或把 `local` 指向容器可达地址，如 docker 网关 `172.17.0.1`）；server 模式直接发布端口即可。

## 运行时连接状态

Windows 上，`status` 还会查询由规范化配置路径派生、受 ACL 保护的**仅本机、只读**命名管道。已认证的本地调用者最多请求一个有界快照，远程客户端被拒绝。`runtime` 是本机快照而非网络探测：包含 schema 版本、角色、进程 ID 与采集时间；client 服务包含 `connecting`、`connected`、`retrying` 或 `stopped`，以及已配置/已解析的**控制通道目标**和连接/错误时间；server 服务包含 `waiting`、`connected` 或 `stopped`，以及已认证控制通道的来源地址和连接/断开元数据。不会暴露 token、密钥、载荷或流量。

进程已停止、仍是旧二进制、使用另一份配置或端点尚未就绪时，`runtime: null` 属于正常结果；`runtime_availability.reason` 给出原因而不会使静态状态查询失败。Linux/systemd 端点仍在计划中。

## CLI quick start (rathole-x)

`rathole-x` 二进制完全基于子命令；裸运行会打印帮助。用 `run -c` 启动守护进程（上游 `./rathole config.toml` 的位置参数形式在本分支不受支持）。

```bash
#（client 添加前需先设置控制通道服务器：
#  `config set --client --remote-addr myserver.com:2333`）
./rathole-x config add --client "name:my_nas_ssh;local:127.0.0.1:22"

# 无需编辑文件即可调整 [client]/[server] 全局字段与传输协议
./rathole-x config set --server --bind-addr 0.0.0.0:2333 --noise            # 生成 noise 密钥对
./rathole-x config set --client --remote-addr myserver.com:2333 --noise-key <SERVER_PUBLIC_KEY>
./rathole-x config set --server --transport tls --pkcs12 identity.pfx --pkcs12-password 1234
./rathole-x config set --client --default-token shared --heartbeat-timeout 60

# 以树形结构显示服务状态与配置（--json 供脚本使用）
./rathole-x status
./rathole-x status --json

# 查看并编辑其维护的配置
./rathole-x config list -c config.toml
./rathole-x config remove my_nas_ssh -c config.toml

# 当缺少 flag 且存在 TTY 时使用交互式提示；
# 脚本可传入所有 flag 并通过 --json 读取机器可读输出。
./rathole-x config add --client "name:my_nas_ssh;local:127.0.0.1:22" --json

# 批量添加多个服务（一次写入；控制通道服务器是全局单值，
# 先用 `config set --client --remote-addr` 设置一次）
./rathole-x config add --client "name:nas;local:127.0.0.1:22" \
  --client "name:db;local:127.0.0.1:5432"

# 生成 noise 密钥对（替代已移除的 --genkey flag）
./rathole-x genkey

# 每个 run 进程只运行一个角色；双段手写配置必须显式选择角色。
./rathole-x run --server -c server.toml
# 安装命名服务：每个服务恰好一个角色，每个服务一个配置文件。
# 自动创建角色专属的骨架配置。
./rathole-x service install server --yes --name relay
./rathole-x service install client --yes --name home-nas

# 授予普通用户对配置的写权限：CLI 在运行时探测权限并跳过 UAC。
# 不存储任何策略文件。
./rathole-x service install server --yes --name relay --allow-user-config

# 卸载：最后一个服务被移除时 version.toml 一并删除；除非给出 --purge，
# 否则配置文件保留。
./rathole-x service uninstall --yes --name relay
./rathole-x service uninstall --yes --purge --name home-nas
```

## CLI reference

`config add|remove|list|set`、`status`、服务生命周期命令和 `upgrade` 都支持 `--json`。JSON 的 stdout 严格只有一个封套：`{ "ok": true, "result": ... }` 或 `{ "ok": false, "error": { "message": ... } }`；进度与诊断不写入 stdout。`config remove`、service install/uninstall 和 upgrade 需要确认；JSON 或无人值守时必须传 `--yes`，否则返回可执行的错误。

- `run [-c CONFIG] [--server|--client]` — 只运行一个守护角色；双段配置必须显式给出 `--server` 或 `--client`。
- `config add [<NAME>] [--client SPEC]... [--server SPEC]... [--bind-addr A] [--local-addr A] [--token T] [--noise] [--noise-key K] [--type tcp|udp] [-c] [--name N] [--json] [--yes]` — 一等、可重复的转发服务创建命令。client SPEC 使用 `name`、`local`、`token`、`type`；每个 client 配置先用一次 `config set --client --remote-addr A` 设置全局控制通道地址。
- `config remove <NAME> [-c | --name N] [--json] [--yes]` — 确认后移除服务。
- `config list [-c] [--name N] [--json]` — 列出服务。
- `config set <--client|--server> [global fields] [-c] [--name N] [--json]` — 设置全局字段：`--remote-addr`、`--bind-addr`、`--default-token`、`--prefer-ipv6`、`--heartbeat-timeout`、`--retry-interval`、`--heartbeat-interval`、`--transport tcp|tls|noise|websocket`、`--noise`、`--noise-key`、`--trusted-root`、`--hostname`、`--pkcs12`、`--pkcs12-password`、`--ws-tls`、`--nodelay`、`--keepalive-secs`、`--keepalive-interval`、`--proxy`。
- `status [-c] [--name N] [--json]` — 打印服务状态与配置树；无参数列出全部服务，`--name N` 查看单服务，`--json` 供脚本使用。
- `genkey [--curve x25519|x448]` — 生成 noise 密钥对。
- `service install <server|client> --yes [-c] [--name N] [--allow-user-config]` — 安装系统服务（二选一角色）：缺失时创建角色骨架配置、将二进制复制到配置旁、写入 `uninstall-<N>.bat`，并以 UAC 注册 Windows SCM 服务（AutoStart）。`--name` 缺省为 "default"。交互式确认后运行；`--yes` 跳过确认（非交互 shell 必传）。
- `service uninstall --yes [--name N] [-c] [--purge] [--all]` — 卸载命名服务；`--all` 移除所有已安装服务与共享二进制但保留配置，`--all --purge` 同时删除全部配置；最后一个服务被移除时 `version.toml` 一并删除。
- `service start|stop|restart [--name N | --all]` — 驱动已安装服务的 SCM 状态（需要时 UAC 提权）。
- `upgrade --yes` — 原地更新已安装的二进制：停止所有服务、用当前运行的二进制替换共享二进制、再重新启动它们。

---

> 以下是以前的 README(原版 rathole 文档)

---

<!-- TOC -->

- [rathole](#rathole)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Configuration](#configuration)
    - [Logging](#logging)
    - [Tuning](#tuning)
  - [Benchmark](#benchmark)
  - [Development Status](#development-status)
- [rathole-x (fork)](#rathole-x-fork)

<!-- /TOC -->

## Features

- **高性能** 具有更高的吞吐量，高并发下更稳定。见[Benchmark](#benchmark)
- **低资源消耗** 内存占用远低于同类工具。见[Benchmark](#benchmark)。[二进制文件最小](docs/build-guide.md)可以到 **~500KiB**，可以部署在嵌入式设备如路由器上。
- **安全性** 每个服务单独强制鉴权。Server 和 Client 负责各自的配置。使用 Noise Protocol 可以简单地配置传输加密，而不需要自签证书。同时也支持 TLS。
- **热重载** 支持配置文件热重载，动态修改端口转发服务。HTTP API 正在开发中。

## Quickstart

一个全功能的 `rathole` 可以从 [release](https://github.com/rapiz1/rathole/releases) 页面下载。或者 [从源码编译](docs/build-guide.md) **获取其他平台和最小化的二进制文件**。

`rathole` 的使用和 frp 非常类似，如果你有后者的使用经验，那配置对你来说非常简单，区别只是转发服务的配置分离到了服务端和客户端，并且必须要设置 token。

使用 rathole 需要一个有公网 IP 的服务器，和一个在 NAT 或防火墙后的设备，其中有些服务需要暴露在互联网上。

假设你在家里的 NAT 后面有一个 NAS，并且想把它的 ssh 服务暴露在公网上：

1. 在有一个公网 IP 的服务器上

创建 `server.toml`，内容如下，并根据你的需要调整。

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333" # `2333` 配置了服务端监听客户端连接的端口

[server.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # 用于验证的 token
bind_addr = "0.0.0.0:5202" # `5202` 配置了将 `my_nas_ssh` 暴露给互联网的端口
```

然后运行:

```bash
./rathole server.toml
```

2. 在 NAT 后面的主机（你的 NAS）上

创建 `client.toml`，内容如下，并根据你的需要进行调整。

```toml
# client.toml
[client]
remote_addr = "myserver.com:2333" # 服务器的地址。端口必须与 `server.bind_addr` 中的端口相同。
[client.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # 必须与服务器相同以通过验证
local_addr = "127.0.0.1:22" # 需要被转发的服务的地址
```

然后运行：

```bash
./rathole client.toml
```

3. 现在 `rathole` 客户端会连接运行在 `myserver.com:2333`的 `rathole` 服务器，任何到 `myserver.com:5202` 的流量将被转发到客户端所在主机的 `22` 端口。

所以你可以 `ssh myserver.com:5202` 来 ssh 到你的 NAS。

[Systemd examples](./examples/systemd) 中提供了一些让 `rathole` 在 Linux 上作为后台服务运行的配置示例。

## Configuration

如果只有一个 `[server]` 和 `[client]` 块存在的话，`rathole` 可以根据配置文件的内容自动决定在服务器模式或客户端模式下运行，就像 [Quickstart](#quickstart) 中的例子。

但 `[client]` 和 `[server]` 块也可以放在一个文件中。然后在服务器端，运行 `rathole --server config.toml`。在客户端，运行 `rathole --client config.toml` 来明确告诉 `rathole` 运行模式。

**推荐首先查看 [examples](./examples) 中的配置示例来快速理解配置格式**，如果有不清楚的地方再查阅完整配置格式。

关于如何配置 Noise Protocol 和 TLS 来进行加密传输，参见 [Transport](./docs/transport.md)。

下面是完整的配置格式。

```toml
[client]
remote_addr = "example.com:2333" # Necessary. The address of the server
default_token = "default_token_if_not_specify" # Optional. The default token of services, if they don't define their own ones
heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. The value must be greater than `server.heartbeat_interval`. Default: 40 seconds
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: 1 second

[client.transport] # The whole block is optional. Specify which transport to use
type = "tcp" # Optional. Possible values: ["tcp", "tls", "noise"]. Default: "tcp"

[client.transport.tcp] # Optional. Also affects `noise` and `tls`
proxy = "socks5://user:passwd@127.0.0.1:1080" # Optional. The proxy used to connect to the server. `http` and `socks5` is supported.
nodelay = true # Optional. Override the `client.transport.nodelay` per service
keepalive_secs = 20 # Optional. Specify `tcp_keepalive_time` in `tcp(7)`, if applicable. Default: 20 seconds
keepalive_interval = 8 # Optional. Specify `tcp_keepalive_intvl` in `tcp(7)`, if applicable. Default: 8 seconds

[client.transport.tls] # Necessary if `type` is "tls"
trusted_root = "ca.pem" # Necessary. The certificate of CA that signed the server's certificate
hostname = "example.com" # Optional. The hostname that the client uses to validate the certificate. If not set, fallback to `client.remote_addr`

[client.transport.noise] # Noise protocol. See `docs/transport.md` for further explanation
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional

[client.transport.websocket] # Necessary if `type` is "websocket"
tls = true # If `true` then it will use settings in `client.transport.tls`

[client.services.service1] # A service that needs forwarding. The name `service1` can change arbitrarily, as long as identical to the name in the server's configuration
type = "tcp" # Optional. The protocol that needs forwarding. Possible values: ["tcp", "udp"]. Default: "tcp"
token = "whatever" # Necessary if `client.default_token` not set
local_addr = "127.0.0.1:1081" # Necessary. The address of the service that needs to be forwarded
nodelay = true # Optional. Determine whether to enable TCP_NODELAY for data transmission, if applicable, to improve the latency but decrease the bandwidth. Default: true
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: inherits the global config

[client.services.service2] # Multiple services can be defined
local_addr = "127.0.0.1:1082"

[server]
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be change.
default_token = "default_token_if_not_specify" # Optional
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeat. Set to 0 to disable sending heartbeat. Default: 30 seconds

[server.transport] # Same as `[client.transport]`
type = "tcp"

[server.transport.tcp] # Same as the client
nodelay = true
keepalive_secs = 20
keepalive_interval = 8

[server.transport.tls] # Necessary if `type` is "tls"
pkcs12 = "identify.pfx" # Necessary. pkcs12 file of server's certificate and private key
pkcs12_password = "password" # Necessary. Password of the pkcs12 file

[server.transport.noise] # Same as `[client.transport.noise]`
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"

[server.transport.websocket] # Necessary if `type` is "websocket"
tls = true # If `true` then it will use settings in `server.transport.tls`

[server.services.service1] # The service name must be identical to the client side
type = "tcp" # Optional. Same as the client `[client.services.X.type]
token = "whatever" # Necessary if `server.default_token` not set
bind_addr = "0.0.0.0:8081" # Necessary. The address of the service is exposed at. Generally only the port needs to be change.
nodelay = true # Optional. Same as the client

[server.services.service2]
bind_addr = "0.0.0.1:8082"
```

### Logging

`rathole`，像许多其他 Rust 程序一样，使用环境变量来控制日志级别。

支持的 Logging Level 有 `info`, `warn`, `error`, `debug`, `trace`

比如将日志级别设置为 `error`:

```shell
RUST_LOG=error ./rathole config.toml
```

如果 `RUST_LOG` 不存在，默认的日志级别是 `info`。

### Tuning

从 v0.4.7 开始, rathole 默认启用 TCP_NODELAY。这能够减少延迟并使交互式应用受益，比如 RDP，Minecraft 服务器。但它会减少一些带宽。

如果带宽更重要，比如网盘类应用，TCP_NODELAY 仍然可以通过配置 `nodelay = false` 关闭。

## Benchmark

rathole 的延迟与 [frp](https://github.com/fatedier/frp) 相近，在高并发情况下表现更好，能提供更大的带宽，内存占用更少。

关于测试进行的更多细节，参见单独页面 [Benchmark](./docs/benchmark.md)。

**但是，不要从这里得出结论，`rathole` 能让内网转发出来的服务快上数倍。** Benchmark 是在本地回环上进行的，其结果说明了任务受 CPU 限制时的结果。当用户的网络不是瓶颈时，用户能得到很大的提升。但是，对很多用户来说并不是这样。在这种情况下，`rathole` 能带来的主要好处是更少的资源占用，而带宽和延迟不一定有显著的改善。

![http_throughput](./docs/img/http_throughput.svg)
![tcp_bitrate](./docs/img/tcp_bitrate.svg)
![udp_bitrate](./docs/img/udp_bitrate.svg)
![mem](./docs/img/mem-graph.png)

## Development Status

`rathole` 正在积极开发中

- [x] 支持 TLS
- [x] 支持 UDP
- [x] 热重载
- [ ] 用于配置的 HTTP APIs

[Out of Scope](./docs/out-of-scope.md) 列举了没有计划开发的特性并说明了原因。
