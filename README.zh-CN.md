# css

`css` 是一个小型 Rust CLI 工具，用于在 Linux 主机上发现、检查并恢复活跃的 Codex CLI 会话。

该工具会从 `~/.codex/state_5.sqlite` 读取 Codex 会话元数据，将其与 `/proc` 中正在运行的 `codex` 进程匹配，并可以生成或执行对应的 `codex resume` 命令。

## 功能

- 列出当前正在运行的 Codex CLI 进程。
- 推断每个进程保存的 Codex 线程/会话元数据。
- 显示会话看起来是在工作中、等待用户输入，还是出现故障。
- 通过会话 ID、标题、PID 或当前工作目录恢复会话。
- 通过终止原始进程并在当前终端中从原会话工作目录恢复会话来接管正在运行的会话。
- 可选地通过本地 Unix socket 守护进程暴露 list/resolve/takeover 操作。

## 要求

- Linux（使用 `/proc` 进行进程发现）
- Rust 工具链
- 已安装 Codex CLI，并可通过 `codex` 命令访问

## 构建

```bash
cargo build --release
```

编译后的二进制文件会写入：

```bash
target/release/css
```

## 用法

列出正在运行的 Codex 会话：

```bash
css list
```

输出 JSON：

```bash
css list --json
css status --json
```

恢复会话：

```bash
css continue <session-id>
css continue <pid>
css continue :cwd
```

向恢复的会话传入初始提示词：

```bash
css continue <session-id> -- "continue the previous task"
```

预览恢复命令但不执行：

```bash
css continue <session-id> --dry-run
# 输出：cd <session-cwd> && codex resume <session-id> --cd <session-cwd>
```

接管正在运行的会话：

```bash
css takeover <pid>
css takeover :cwd
```

运行本地守护进程：

```bash
css serve
css serve --socket /tmp/css.sock
```

## 配置

使用自定义 Codex 主目录：

```bash
css --codex-home /path/to/.codex list
```

或设置：

```bash
CODEX_HOME=/path/to/.codex css list
```

使用自定义 Codex 二进制文件：

```bash
css --codex-bin /path/to/codex continue <session-id>
```

## 许可证

本项目基于 MIT License 授权。详见 [LICENSE](LICENSE)。
