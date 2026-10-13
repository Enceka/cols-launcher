# COLS 启动器

独立于 `COLS` 的 Rust 工程，提供 egui 原生窗口、只读检查、启动参数预览、客户端启动和本地 TCP 字节转发。运行时不依赖 Python。不包含游戏程序、资源、补丁、注入模块或原启动器代码。

**WFP 请求拦截尚未实现，未经配置变更的原生 CODOL 登录尚未验证。** TCP 转发器可以运行，但连接必须已指向监听端口，或由外部适配器送到监听端口。TLS 验证仍由客户端执行。设计与实际证据见 [docs/DESIGN.md](docs/DESIGN.md)。

## 构建与运行

需要 Rust 1.96+。默认编译原生 GUI；仅使用命令行时可加 `--no-default-features`。

```sh
cargo build --locked --release
cargo run --locked -- gui --config profiles/codol3333.example.json
cargo run --locked -- plan --config profiles/codol3333.example.json
cargo test --locked --no-default-features
```

本机 Homebrew Rust 与 rustup 路径冲突时，可用 `sh tools/cargo.sh` 替代 `cargo`。例如：

```sh
sh tools/cargo.sh run --locked -- gui --config profiles/codol3333.example.json
```

构建出的原生程序是 `target/release/codol-launcher`，Windows 为 `codol-launcher.exe`。构建和运行分开；运行不需要安装 Rust。GUI 支持输入或拖入配置 JSON 和客户端程序路径；中文字体从本机系统读取，不打包游戏字体。

`plan` / `inspect` 不启动客户端、不连接上游、不创建状态文件。外部拦截示例会显示尚未实现的阻塞，返回码为 2。复制示例为 `profiles/research.local.json`，调整路径后运行 `launch --config profiles/research.local.json`。

路径相对 **JSON 所在目录** 解析，配置不能放在客户端目录内。`sha256` 是所选 EXE 的校验基线，示例摘要不证明其来源或授权；`arguments` 是独立参数数组，不经过 shell，也不应放入账号密码或官方令牌。

## 指定联机服务器

GUI 可填写服务器 IP 或域名，分别指定 AUTH、LOBBY 端口；留空沿用配置。点击“保存联机配置”写入相邻的 `*.local.json`，下次可直接加载，不改写游戏文件。

```sh
cargo run --locked -- gui --config profiles/multiplayer.example.json
cargo run --locked -- plan --config profiles/multiplayer.example.json --server localhost --auth-port 39002 --lobby-port 39001
cargo run --locked -- launch --config profiles/multiplayer.example.json --server 127.0.0.1
```

`localhost`、`127.0.0.1` 是占位地址，须替换为自己的服务器。上游接受 IPv4、IPv6 或 ASCII 域名，国际化域名使用 punycode。域名在建立连接时解析，不修改 hosts；预览不查询 DNS。解析结果指向本机转发监听端口时会拒绝，防止循环。

这些设置选择 TCP 上游，不能替代尚未完成的原始连接拦截。玩家联机还需服务端对外监听，并向客户端通告可达的游戏/STUN 地址；转发认证和大厅不等于 UDP 对局已连通。当前服务端的示例绑定多为回环地址，不能直接作为公网联机配置。

Windows 直接启动用户提供的 EXE。macOS/Linux 使用已有 Wine 时，配置 `client.runner` 为 Wine 程序绝对路径，`wine_prefix` 为已有且位于客户端目录外的 prefix。启动器不安装 Wine、不创建 prefix。v0 只追踪并停止直接启动的进程，不支持启动后立即退出的分离式包装器，不保证停止所有 Wine 原生子进程。

## 连接模式

| `routing` | 行为 | 状态 |
| --- | --- | --- |
| `direct` | 按原有配置启动客户端；`routes` 为空 | 已实现，不改变网络路径 |
| `existing-config` | 只读解析现有 `dwauth.ini`，AUTH/LOBBY 必须匹配转发监听端口 | 已实现检查及 TCP 转发，不改写 INI |
| `external` | 由进程外适配器将原目的连接重定向至转发器 | 设计阶段，`launch` 拒绝启动 |

`profiles/existing-config.example.json` 假设用户现有文件已指向 `127.0.0.1:39001/39002`；转发器监听这两个端口，上游服务端需另行监听 `49001/49002`。同一地址/端口不能同时运行原服务端。检查不匹配时会拒绝启动，不自动修改客户端。

AUTH 转发不终止 TLS、不导入 CA、不关闭验证；不能解决自签名证书或主机名不匹配。IIPS、UDP/STUN 3074/3078 及游戏 UDP 需另具可达服务端地址，v0 不转发这些通道。检查通过只表示可以尝试启动，不表示完整登录或对局成功。

仅运行 TCP 转发器，由外部测试程序连接时：

```sh
cargo run --locked --no-default-features -- relay --config profiles/codol3333.example.json
```

Ctrl-C 关闭。此命令不启动客户端，也不截获其他连接。只监听明确的 IP 回环地址，每条规则固定一个上游；最多 64 个并发连接，DNS 解析及上游连接分别超时 5 秒，每个方向空闲超时 120 秒。不记录请求体、凭证或 TLS 明文。

## 文件和会话

启动器仅写客户端目录之外的 `state_dir`：会话 JSON、最多 256 KiB 的连接元数据日志和锁。同一状态目录下，同一个客户端不能同时启动两次；第三方启动器及不同状态目录不在锁覆盖范围内。不按名称批量结束其他进程。

启动前后核对 EXE、连接配置、CA 文件及少量 DLL；范围见 `src/inspection.rs`，不涵盖全部资源。差异记入会话记录，不自动回滚。客户端自己的存档、日志和更新仍由客户端决定；启动器不改写文件，不代表原程序运行时不会写盘。

测试使用合成文件和本项目自己的测试程序，不需要商业游戏资源。覆盖大数据双向转发、半关闭、端口绑定失败、上游不可达、停止清理、摘要、目录隔离及重复会话。Windows、Wine、真实游戏登录及 UDP 对局尚未实机验收。

## 发布范围

按用户确定的范围，仅提供服务端和启动器。客户端及资源由用户自行合法取得；不提供资源下载、自动补丁或客户端更新。启动器代码采用 [MIT License](LICENSE)，授权范围不包含原游戏程序或资源。本项目无官方关联或背书。

重写客户端并读取用户本地资源是尚未实施的替代方向。服务端政策见相邻仓库 [LEGAL.md](https://github.com/Enceka/cols/blob/main/LEGAL.md)；启动器自身发行以本文件和 LICENSE 为准，不依赖相邻目录存在。
