# 片刻 (pianke) Tauri 桌面应用

Tauri 2 实现的桌面应用包装器，嵌入 Python 后端实现图片处理功能。

## 目录结构

```
src-tauri/
├── Cargo.toml          # Rust 依赖配置
├── tauri.conf.json     # Tauri 应用配置
├── build.rs            # 构建脚本
├── src/
│   ├── main.rs         # 应用入口
│   ├── commands.rs     # Tauri 命令定义（供前端调用）
│   ├── launcher.rs      # Python 应用启动器
│   ├── python_runtime.rs # Python 运行时管理
│   ├── env_check.rs    # 环境检查
│   └── updater.rs      # 自动更新
├── frontend/
│   └── index.html      # 前端入口
├── capabilities/       # Tauri 2 权限配置
├── gen/                # 生成的代码
└── icons/              # 应用图标
```

## 模块说明

| 模块 | 功能 |
|------|------|
| `main.rs` | 应用入口，窗口创建，命令注册 |
| `commands.rs` | 定义前端调用 Rust 的 IPC 命令 |
| `launcher.rs` | 启动 Python 后端进程 |
| `python_runtime.rs` | Python 运行时初始化和管理 |
| `env_check.rs` | 检查系统环境和依赖 |
| `updater.rs` | 应用自动更新逻辑 |

## 运行方式

### 开发模式

```bash
# 安装 Tauri CLI (如未安装)
cargo install tauri-cli

# 在项目根目录运行
cargo tauri dev
```

### 构建

```bash
cargo tauri build
```

## 依赖要求

- Rust 1.70+
- Node.js 18+
- Python 3.10+ (运行时由 `app.py` 提供)
- Tauri CLI 2.x

## 配置

关键配置在 `tauri.conf.json`:
- `productName`: 片刻
- `identifier`: com.pianke.desktop
- `frontendDist`: 前端资源目录 (./frontend)
- `windows`: 窗口配置 (1200x800, 居中显示)