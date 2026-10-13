# COLS 启动器

独立于 `COLS` 的 Rust 启动器，接入用户本地的原生客户端，保留原生画面和玩法。提供本地服务端管理、原生程序启动、连接配置与本地 CA 的临时调整及恢复。运行时不依赖 Python，不包含游戏程序、资源、补丁、注入模块或原启动器代码。

本机原生模式保持 EXE、DLL 和游戏资源不变，允许临时调整连接 INI 和附加本地 CA，退出后恢复。WFP 请求拦截及公网联机尚未实现；原有 TCP 转发模式仍可单独使用。设计见 [docs/DESIGN.md](docs/DESIGN.md)。

## 构建与运行

需要 Rust 1.96+。默认编译原生 GUI；仅使用命令行时可加 `--no-default-features`。

```sh
cargo build --locked --release
cargo run --locked -- gui
cargo run --locked -- plan --config profiles/codol3333.example.json
cargo test --locked --no-default-features
```

本机 Homebrew Rust 与 rustup 路径冲突时，可用 `sh tools/cargo.sh` 替代 `cargo`。例如：

```sh
sh tools/cargo.sh run --locked -- gui --config profiles/codol3333.example.json
```

构建出的程序是 `target/release/cols-launcher`，Windows 为 `cols-launcher.exe`。运行不需要安装 Rust。GUI 提供“本机原生游戏”和“连接转发”，支持输入或拖入配置 JSON 和程序路径；中文字体从本机系统读取，不打包游戏字体。

## 本机原生游戏

工作区需要用户自行提供的 `CODOL/` 客户端、已构建的 `server/target/release/codol-server`、本地证书/私钥、服务资料以及已准备的地图状态和目录。启动器不自动下载客户端或地图资源，不调用 `play.py` / `gamehub_run.py`。已有资料的准备要求见服务端的本地游玩文档。

在本仓库中：

```sh
sh tools/cargo.sh build --release --locked
target/release/cols-launcher native-init --workspace ..
target/release/cols-launcher native-plan
target/release/cols-launcher native-launch
```

可用 `native-launch --seconds 90` 做有时限的启动验证，默认持续到游戏退出。时限从启动流程开始计算，包含地图加载和客户端启动。

`native-init` 生成 `profiles/native.local.json`，不覆盖已有配置；路径指向指定工作区。在 macOS 上会读取已有 GameHub Wine10 安装信息和现有游玩 prefix，不安装或重建运行环境。Windows 直接启动 EXE，服务端路径须对应自己的 Windows 构建；其它 Wine 环境可在 JSON 中配置。GUI 的“生成本机配置”提供同样操作。

`native-plan` 只检查文件、基线、服务资料和原生进程占用；不启动服务、不连接网络、不修改客户端。如果已有游戏运行，会明确拒绝再次启动。`native-launch` 管理本次 AUTH、LOBBY、STUN、游戏 UDP 和 IIPS 资源观察端点；等到服务端报告地图与各端点就绪才临时配置并启动原生程序。

macOS 上先检查 Wine 是否能执行基础命令。若通信锁确由当前用户已进入退出状态、RSS 为零的旧 wineserver 持有，会保留并隔离旧通信目录，再尝试一次；活动进程或内核 I/O 等待不会被当成失效锁处理。对标记匹配的项目专用 prefix，退出时核实 wineserver 身份并执行限定该 prefix 的关闭，管理命令和进程等待都有超时。诊断与恢复边界见 [docs/WINE_RECOVERY.md](docs/WINE_RECOVERY.md)。

启动前备份 `dwauth.ini`、`stun.ini`、`iips.ini`、`cacert.pem` 和 `IIPS/cacert.pem`，只把配置中指定的本地证书追加到客户端 CA bundle，不关闭验证、不导入系统信任库。游戏结束、Ctrl-C 或窗口关闭后，停止本次进程并恢复原文件；原来不存在的文件会删除。不会回滚游戏自己的存档、玩家设置或正常服务器资料。

按原生入口进入：玩 → 多人竞技 → 创建房间 → 战术团队竞技，选择已准备地图并准备，服务端模拟的 Bot 加入。本机使用现有账号资料，地图和玩法范围由服务端准备决定。

异常中断后：

```sh
target/release/cols-launcher native-recover
```

恢复依据外部 `state_dir` 中的持久化记录与备份，验证进程 PID、创建时间及程序路径。活跃的原启动器会话不能被恢复命令抢占；外部改动过的配置保留备份并拒绝覆盖。若异常发生在分离式 Wine 程序刚启动而尚未记录真实 PID 的窗口，需先退出该游戏，再恢复。不要删除待恢复记录或手动覆盖备份。

本地管理已用合成原生程序验证启动、分离式包装器、就绪、失败、取消、配置恢复及占用保护。本机 macOS/Wine10 原生样本已通过 Rust 启动器完成认证、大厅、开房、阿富汗地图进入、移动、射击、换弹及结算；退出后受检文件摘要一致，连接配置和 CA 已恢复。该验收使用独立服务资料，不代表 Windows 或公网联机已通过。

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

Windows 直接启动用户提供的 EXE。macOS/Linux 配置现有 Wine 路径和位于客户端目录外的 prefix，启动器不安装 Wine、不创建 prefix。转发模式只追踪直接启动的进程；本机原生模式会识别真实游戏 PID，支持正常退出的启动包装器。macOS 项目专用 prefix 支持会话整体关闭；其它环境按记录的进程身份清理。

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

转发模式仅写外部 `state_dir`，连接元数据日志最多 256 KiB。本机原生模式另外临时调整上述五个配置/CA 文件，使用跨状态目录的客户端锁；若已有研究工具的共享锁，也会获取该锁。原生进程占用检查在修改配置前执行，不按程序名称批量结束其他进程。

启动前后核对 EXE、连接配置、CA 文件及少量 DLL；范围见 `src/inspection.rs`，不涵盖全部资源。只恢复启动器临时配置的五个文件；EXE/DLL/资源没有修改入口。客户端自己的存档、日志和更新仍由客户端决定。

自动测试使用合成文件和本项目自己的测试程序，不需要商业游戏资源。覆盖大数据双向转发、半关闭、端口绑定失败、上游不可达、停止清理、摘要、目录隔离、重复会话、Wine 失效锁的判定边界及客户端大量输出的有界记录。实机范围见上文；Windows 和公网联机尚未验收。

## 发布范围

按用户确定的范围，仅提供服务端和启动器。客户端及资源由用户自行合法取得；不提供资源下载、自动补丁或客户端更新。启动器代码采用 [MIT License](LICENSE)，授权范围不包含原游戏程序或资源。本项目无官方关联或背书。

本项目采用原生客户端接入路线，不实现游戏引擎。服务端政策见 [LEGAL.md](https://github.com/Enceka/cols/blob/main/LEGAL.md)；启动器自身发行以本文件和 LICENSE 为准。
