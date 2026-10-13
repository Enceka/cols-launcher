# macOS Wine 会话恢复

`native-launch` 在启动本地服务和修改客户端配置之前，对配置中的 Wine prefix 执行 `wine cmd /d /c exit 0`，最长等待 12 秒。检查只用于 Wine 原生模式；Windows 直接启动不走此路径。

## 失效通信锁

Wine10 按当前用户和 prefix 的设备号、inode 定位 `/private/tmp/.wine-UID/server-DEV-INO`。启动器持有 prefix 独占锁，通过 `F_GETLK` 查询该目录中记录锁的实际持有 PID。基础命令失败或超时时，只有同时满足以下条件才会自动恢复：

- 通信目录、锁的类型与所有者匹配，目录没有额外文件或符号链接。
- 实际锁持有 PID 与 Wine 错误或查询结果一致。
- `ps` 确认该 PID 属于当前用户、程序为 wineserver、RSS 为零，且状态含 macOS 的 `E`（正在退出）。

恢复将原目录重命名为同级 `.cols-retired-时间戳`，保留锁和故障证据，再执行一次基础检查。它不删除 prefix、注册表、存档或资源。活动 wineserver、身份不明的持有者和 `U`（不可中断等待）状态均拒绝自动隔离。

`wine-runtime.json` 保存基础检查和隔离结果。旧内核退出条目可能继续显示在 `ps` 中；恢复成功表示新的 Wine 会话可运行，并不表示旧条目已被杀死。

## 退出与日志

仅当 prefix 的 `.codol-prefix.json` 标记为 `purpose="CODOL research"`，且 `engine_id` 匹配配置的 Wine 安装目录时，启动器管理整个 prefix 的退出。关闭前再次确认记录的 wineserver 身份和当前锁持有者，然后用明确的 `WINEPREFIX` 执行对应安装的 `wineserver -k` 和有界 `-w`。不使用全局 `killall`。

直接客户端和服务进程也有终止后的等待上限。超时会记录错误；仍有原生客户端时保留临时配置和恢复记录，避免在游戏运行期间恢复文件。正常退出恢复五个连接配置/CA 文件，可通过 `session.json` 查看 `configs_restored`、`changed_files` 与 `error`。

`client.log` 持续读取两个输出管道，共享 4 MiB 上限。诊断文件访问阻塞时，可在本机配置中设置 `"trace_wine_files": true`，产生 Wine 文件调用跟踪；平常保持关闭。日志和会话资料在忽略目录内，不随程序发布。

## 文件访问阻塞

Wine 基础命令正常、游戏仍停在资源初始化时，应区分通信故障与系统文件访问阻塞。已观察到特定 IFS 文件对象的普通 `open()` 进入内核等待；换 prefix 或重复 SIGKILL 不能解除这种等待。

本机已采用的恢复方法是保留故障文件对象，从用户已有的本地副本读取内容，校验完整 SHA-256 与资源基线一致后写入新文件对象，再验证安装后的摘要。该方法保持文件内容一致，不修补 EXE/DLL，也不提供或下载游戏资源。不要再次打开保留的 `.blocked` 对象。确切的底层文件系统成因尚未确定。

资源对象替换属于本机诊断操作，启动器没有自动改写游戏资源的入口。若已确认旧 prefix 存在不可终止的 I/O 等待，可使用另外准备的专用 prefix；复制 Wine 环境必须保留符号链接，尤其不得跟随 `dosdevices/z:` 遍历系统根目录。

## 验证范围

失效锁恢复已用本机真实的退出残留锁验证；自动测试覆盖活动或外部进程拒绝、目录类型校验和无持有者时拒绝隔离。客户端输出测试通过同时写满 stdout/stderr 验证日志上限和退出恢复。

本机 Wine10 原生客户端已完成认证、大厅、12 人房间、阿富汗地图、移动、弹药消耗、换弹与结算。资源文件对象的处理和 Wine 锁处理是两项独立修复，不能据此保证所有内核 I/O 故障均可由启动器消除。

同一专用环境连续两次启动均进入原生对局，全程未重启系统。第一轮结算后以 SIGINT 结束启动器，第二轮由 300 秒验证时限结束；两轮均成功关闭会话并恢复连接配置与 CA，受检文件没有差异。第二轮再次确认射击和换弹，未取得第二份结算状态记录。修复资源的完整摘要在两轮结束后仍与基线一致。

Wine 的路径和记录锁约定依据 [Wine10 客户端实现](https://github.com/wine-mirror/wine/blob/wine-10.0/dlls/ntdll/unix/server.c) 与 [服务端实现](https://github.com/wine-mirror/wine/blob/wine-10.0/server/request.c)。
