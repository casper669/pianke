//! 运行环境检测 — 检查 Python、磁盘空间、内存是否满足运行要求。
//!
//! 检测结果通过 EnvInfo 结构返回给前端，前端据此决定：
//! - 显示"环境就绪"并允许选择模式
//! - 显示错误信息并引导用户安装 Python
//! - 显示警告（如内存偏小不建议使用专家模式）
//!
//! Python 查找策略（跨平台）：
//! 1. 先用 which/where 搜索 PATH 中的 python3/python
//! 2. 如果找不到，搜索常见安装目录（Homebrew、Windows Python 安装目录）
//! 3. 每个候选都要验证版本 >= 3.10

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// 最低磁盘空间要求（GB），用于存放虚拟环境和 Python 包
const MIN_DISK_GB: f64 = 3.0;
/// 最低内存要求（GB），专家模式需要加载深度学习模型，小于此值会警告
const MIN_MEMORY_GB: f64 = 4.0;

/// 环境检测结果，序列化后直接返回给前端展示。
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvInfo {
    /// Python 可执行文件的绝对路径
    pub python_path: Option<String>,
    /// Python 版本字符串，如 "Python 3.12.3"
    pub python_version: Option<String>,
    /// 当前磁盘剩余空间（GB）
    pub disk_free_gb: f64,
    /// 系统总内存（GB）
    pub memory_gb: f64,
    /// 操作系统名称和架构，如 "macos aarch64"
    pub os_name: String,
    /// 阻塞性错误 — 不解决无法继续（如未找到 Python）
    pub errors: Vec<String>,
    /// 警告性提示 — 不阻塞但建议用户注意（如内存偏小）
    pub warnings: Vec<String>,
}

/// 执行完整的环境检测，返回包含所有诊断信息的 EnvInfo。
///
/// `venv_path` 是可选的 venv Python 路径。如果系统没找到 Python 但 venv 已通过
/// uv 创建好了，会从 venv Python 获取版本号，不会报告"未找到 Python"错误。
pub fn check(venv_path: Option<&PathBuf>) -> EnvInfo {
    let mut info = EnvInfo {
        python_path: None,
        python_version: None,
        disk_free_gb: 0.0,
        memory_gb: 0.0,
        os_name: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        errors: Vec::new(),
        warnings: Vec::new(),
    };

    // 1. 检测 Python — 最关键的一步
    // 优先查系统 Python，找不到时尝试 venv 中由 uv 管理的 Python
    if let Some(path) = find_python() {
        info.python_version = get_python_version(&path).ok();
        info.python_path = Some(path.display().to_string());
    } else if let Some(vp) = venv_path.and_then(|p| {
        if p.as_os_str().is_empty() { None } else { Some(p) }
    }) {
        // venv 已通过 uv 创建好了（系统没有 Python 但 uv 提供了独立的）
        if let Ok(v) = get_python_version(vp) {
            info.python_version = Some(format!("{} (由 uv 管理)", v));
            info.python_path = Some(vp.display().to_string());
        }
    } else {
        let mut msg = String::from("未找到 Python 3.10+。请安装 Python 3.10 或更高版本。\n");
        #[cfg(target_os = "macos")]
        { msg.push_str("macOS: brew install python@3.10\n"); }
        #[cfg(target_os = "linux")]
        { msg.push_str("Linux: 使用包管理器安装，如 apt install python3 或 dnf install python3\n"); }
        #[cfg(target_os = "windows")]
        { msg.push_str("Windows: 从 python.org 下载安装，或使用 winget install Python.Python.3.12\n"); }
        msg.push_str("或从 https://www.python.org/downloads/ 下载");
        info.errors.push(msg);
    }

    // 2. 检测磁盘空间 — 专家模式的 PyTorch 包约 2-3GB
    info.disk_free_gb = get_free_disk_gb();
    if info.disk_free_gb < MIN_DISK_GB {
        info.errors.push(format!(
            "磁盘空间不足（剩余 {:.1} GB，建议至少 {:.0} GB 用于安装依赖）",
            info.disk_free_gb, MIN_DISK_GB
        ));
    }

    // 3. 检测内存 — 专家模式需要加载多个深度学习模型到内存
    info.memory_gb = get_total_memory_gb();
    if info.memory_gb < MIN_MEMORY_GB {
        info.warnings.push(format!(
            "可用内存较小（{:.1} GB），专家模式（深度学习）可能不稳定，建议使用极速模式",
            info.memory_gb
        ));
    }

    info
}

/// 在系统中查找可用的 Python 3.10+。
///
/// 查找顺序：
/// 1. PATH 中搜索 python3 / python（Windows 上还包括 py 启动器）
/// 2. macOS：/opt/homebrew/bin, /usr/local/bin, /usr/bin
/// 3. Windows：%LOCALAPPDATA%\Programs\Python\Python31x\
///
/// 每个找到的候选都会通过 --version 验证版本号 >= 3.10。
pub fn find_python() -> Option<PathBuf> {
    // 根据平台确定搜索的二进制名称
    // Windows 上 py 是 Python 官方启动器，能自动选最新版本
    let names: &[&str] = if cfg!(target_os = "windows") {
        &["python", "python3", "py"]
    } else {
        &["python3", "python"]
    };

    // 第一轮：通过 which/where 搜索 PATH
    for name in names {
        let result = if cfg!(target_os = "windows") {
            Command::new("where").arg(name).output()
        } else {
            Command::new("which").arg(name).output()
        };
        if let Ok(output) = result {
            let p = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !p.is_empty() && check_python_version(&p) {
                return Some(PathBuf::from(p));
            }
        }
    }

    // 第二轮：搜索常见安装目录（PATH 中找不到时的兜底）
    #[cfg(target_os = "macos")]
    {
        let search_paths = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];
        for dir in &search_paths {
            for name in names {
                let p = Path::new(dir).join(name);
                if p.exists() && check_python_version(&p.to_string_lossy()) {
                    return Some(p);
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let search_paths = ["/usr/bin", "/usr/local/bin"];
        for dir in &search_paths {
            for name in names {
                let p = Path::new(dir).join(name);
                if p.exists() && check_python_version(&p.to_string_lossy()) {
                    return Some(p);
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(home) = std::env::var("LOCALAPPDATA") {
            let search_paths = [
                Path::new(&home).join("Programs").join("Python").join("Python312"),
                Path::new(&home).join("Programs").join("Python").join("Python311"),
                Path::new(&home).join("Programs").join("Python").join("Python310"),
            ];
            for dir in &search_paths {
                for name in names {
                    let p = dir.join(name).with_extension("exe");
                    if p.exists() && check_python_version(&p.to_string_lossy()) {
                        return Some(p);
                    }
                }
            }
        }
    }

    None
}

/// 通过执行 `python --version` 验证版本 >= 3.10。
fn check_python_version(path: &str) -> bool {
    Command::new(path)
        .args(["--version"])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            parse_python_version(&s)
        })
        .map(|(maj, min)| maj > 3 || (maj == 3 && min >= 10))
        .unwrap_or(false)
}

/// 解析 Python 版本字符串，如 "Python 3.12.3" → (3, 12)。
///
/// Python --version 输出格式为 "Python X.Y.Z"，取前两位。
pub fn parse_python_version(s: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() >= 2 {
        let version: Vec<&str> = parts[1].split('.').collect();
        if version.len() >= 2 {
            let maj: u32 = version[0].parse().ok()?;
            let min: u32 = version[1].parse().ok()?;
            return Some((maj, min));
        }
    }
    None
}

/// 获取 Python 的完整版本字符串（用于前端展示）。
fn get_python_version(path: &Path) -> Result<String> {
    let output = Command::new(path)
        .args(["--version"])
        .output()
        .context("Failed to run python --version")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 获取当前工作目录所在磁盘的剩余空间（GB）。
///
/// 使用 sysinfo 库查询磁盘信息，匹配当前工作目录所在的挂载点。
fn get_free_disk_gb() -> f64 {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    for disk in disks.list() {
        if cwd.starts_with(disk.mount_point()) {
            return disk.available_space() as f64 / (1024.0 * 1024.0 * 1024.0);
        }
    }
    // 兜底：取第一个磁盘的可用空间
    disks
        .list()
        .first()
        .map(|d| d.available_space() as f64 / (1024.0 * 1024.0 * 1024.0))
        .unwrap_or(0.0)
}

/// 获取系统总内存（GB）。
fn get_total_memory_gb() -> f64 {
    use sysinfo::System;
    let sys = System::new_all();
    sys.total_memory() as f64 / (1024.0 * 1024.0 * 1024.0)
}
