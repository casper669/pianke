//! Python 运行时管理 — 虚拟环境创建、pip 包安装、Flask 服务启动。
//!
//! 核心职责：
//! 1. setup() — 从打包资源中提取 Python 代码，创建隔离的虚拟环境
//! 2. install_packages() — 用 uv（优先）或 pip 安装依赖，带实时进度回调
//! 3. deps_ready() — 快速检查已安装的包是否可导入
//! 4. start_flask() — 启动 Flask 子进程
//!
//! 设计决策：
//! - 优先使用 uv（比 pip 快 10-100 倍），找不到时回退到 pip
//! - venv 使用 uv venv --python ">=3.10"，uv 可自动下载 Python 解释器
//! - 资源提取时跳过 _up_ 目录（Tauri 打包的内部结构）和图标文件

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

use crate::launcher::MirrorConfig;

/// Python 运行时的路径信息。
///
/// venv_python 指向虚拟环境中的 Python 解释器，
/// 所有 pip 安装和 Flask 启动都通过这个解释器执行。
pub struct PythonRuntime {
    /// 虚拟环境中 Python 可执行文件的路径
    pub venv_python: PathBuf,
    /// 代码副本目录（包含 app.py、pic_selecter/、static/ 等）
    pub app_dir: PathBuf,
}

impl PythonRuntime {
    /// 从打包资源中提取代码到数据目录（始终执行，不依赖 Python）。
    ///
    /// 返回 app_dir 路径。代码复制只在首次启动时执行，
    /// 后续启动跳过复制，由更新模块负责增量更新。
    pub fn extract_resources(
        resource_dir: &Path,
        app_data_dir: &Path,
    ) -> Result<PathBuf> {
        let app_dir = app_data_dir.join("app");

        let needs_extract = !app_dir.join("app.py").exists();
        if needs_extract {
            if app_dir.exists() {
                std::fs::remove_dir_all(&app_dir)?;
            }
            std::fs::create_dir_all(&app_dir)?;
            copy_resource_dir(resource_dir, &app_dir)?;
            log::info!("Code extracted to {:?}", app_dir);
        }

        Ok(app_dir)
    }

    /// 创建虚拟环境（如果不存在）并返回 venv Python 路径。
    ///
    /// `use_ensure_uv` 控制是否允许自动下载安装 uv：
    /// - false（快速模式）：只搜索已有 uv，找不到直接回退系统 Python
    /// - true（完整模式）：没 uv 时从网络下载安装（可能需要 30 秒+）
    ///
    /// 在 main.rs setup 阶段用 false（快速），在 init_setup 命令中可重试用 true。
    pub fn setup_venv(
        python_path: Option<&Path>,
        app_data_dir: &Path,
        use_ensure_uv: bool,
        on_progress: impl Fn(&str),
    ) -> Result<PathBuf> {
        let venv_dir = app_data_dir.join("venv");

        // 确定 venv 中 Python 的路径（Windows 和 Unix 路径不同）
        let venv_python = if cfg!(target_os = "windows") {
            venv_dir.join("Scripts").join("python.exe")
        } else {
            venv_dir.join("bin").join("python3")
        };

        // venv 不存在时创建
        if !venv_python.exists() {
            log::info!("Creating venv at {:?} (ensure_uv={})", venv_dir, use_ensure_uv);
            create_venv(python_path, &venv_dir, use_ensure_uv, on_progress)?;
        }

        Ok(venv_python)
    }

    /// 安装 pip 包到虚拟环境，通过回调实时报告进度。
    ///
    /// 安装策略：
    /// - 优先使用 uv pip install（Rust 实现，并行下载，比 pip 快很多）
    /// - uv 未安装时回退到标准 pip
    /// - 使用镜像源加速（国内默认清华大学 TUNA）
    /// - 安装完成后自动修复 OpenCV 包冲突
    ///
    /// 进度解析：同时兼容 uv 和 pip 的输出格式。
    /// uv 输出：Resolved X packages / Prepared X packages / Installed X packages
    /// pip 输出：Collecting X / Downloading X / Installing collected packages
    pub fn install_packages(
        &self,
        packages: &[String],
        mirror: &MirrorConfig,
        on_progress: impl Fn(&str),
    ) -> Result<()> {
        if packages.is_empty() {
            return Ok(());
        }

        let total = packages.len();
        let uv = ensure_uv(&on_progress);
        let using_uv = uv.is_some();

        if using_uv {
            on_progress(&format!(
                "使用 uv 安装 {} 个依赖包（速度更快）...",
                total
            ));
        } else {
            on_progress(&format!("开始安装 {} 个依赖包...", total));
        }

        // 构建安装命令
        // uv: uv pip install --python <venv_python> [mirror args] <packages>
        // pip: <venv_python> -m pip install [mirror args] <packages>
        let mut cmd = if let Some(ref uv_path) = uv {
            let mut c = Command::new(uv_path);
            c.args(["pip", "install", "--python"])
                .arg(&self.venv_python);
            c
        } else {
            let mut c = Command::new(&self.venv_python);
            c.args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input"]);
            c
        };

        // 配置镜像源
        if mirror.use_mirror {
            cmd.arg("-i").arg(&mirror.pypi_index);
            cmd.arg("--extra-index-url").arg(&mirror.pypi_extra);
            if using_uv {
                on_progress(&format!("使用镜像源: {}", mirror.pypi_label));
            } else {
                on_progress(&format!("使用镜像源: {}（uv 未安装，用 pip 较慢）", mirror.pypi_label));
            }
        }

        for pkg in packages {
            cmd.arg(pkg);
        }

        // 捕获 stderr — pip 和 uv 都把进度输出到 stderr
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to start installer")?;

        // 逐行读取 stderr 并解析进度信息
        let stderr = child.stderr.take().unwrap();
        let reader = BufReader::new(stderr);

        let mut installed = 0usize;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // uv 的进度输出格式
            if trimmed.starts_with("Resolved") && using_uv {
                on_progress(trimmed);
            } else if trimmed.starts_with("Prepared") && using_uv {
                on_progress(trimmed);
            } else if trimmed.starts_with("Installed") && using_uv {
                on_progress(trimmed);
            }

            // pip 的进度输出格式
            if trimmed.starts_with("Collecting ") {
                let pkg_info = trimmed.strip_prefix("Collecting ").unwrap_or(trimmed);
                installed += 1;
                on_progress(&format!("[{}/{}] 下载 {}", installed, total, pkg_info));
            } else if trimmed.starts_with("Downloading ") {
                on_progress(trimmed);
            } else if trimmed.starts_with("Installing collected packages") {
                on_progress("正在安装已下载的包...");
            } else if trimmed.starts_with("Successfully installed") {
                on_progress(trimmed);
            }
            log::debug!("install: {}", trimmed);
        }

        // 排空 stdout（避免子进程阻塞）
        if let Some(stdout) = child.stdout.take() {
            for _ in BufReader::new(stdout).lines() {}
        }

        let status = child.wait().context("Install process failed")?;
        if !status.success() {
            anyhow::bail!("Install returned non-zero exit: {}", status);
        }

        // 修复 OpenCV 冲突：如果同时安装了 opencv-python 和 opencv-contrib-python，
        // 需要卸载前者保留后者（contrib 包含了完整功能）
        on_progress("检查 OpenCV 兼容性...");
        self.fix_opencv_conflict(uv.as_deref())?;

        on_progress("依赖安装完成");
        Ok(())
    }

    /// 快速检查依赖是否已就绪。
    ///
    /// 通过 Python import 语句验证关键包能否成功导入。
    /// 这比检查文件系统更可靠，因为能同时验证包和其传递依赖的完整性。
    /// 不检查所有包，只检查最具代表性的几个来加速检测。
    pub fn deps_ready(&self, modes: &[String]) -> bool {
        // 核心包的代表性导入
        let mut imports = vec![
            "flask", "PIL", "numpy", "cv2", "imagehash", "rawpy", "piexif",
        ];
        // 专家模式额外的代表性导入
        if modes.iter().any(|m| m == "expert") {
            imports.extend(["torch", "transformers", "insightface", "pyiqa"]);
        }
        // 土豪模式额外的代表性导入
        if modes.iter().any(|m| m == "tycoon") {
            imports.push("openai");
        }
        let check = imports.join(", ");
        Command::new(&self.venv_python)
            .args(["-c", &format!("import {}", check)])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// 启动 Flask 服务子进程。
    ///
    /// 启动参数：
    /// - --port: 监听端口（默认 5057）
    /// - --no-browser: 不自动打开浏览器（Tauri 负责导航）
    ///
    /// 环境变量：
    /// - HF_ENDPOINT: HuggingFace 镜像（国内加速下载模型）
    ///
    /// 返回子进程句柄，调用方负责管理其生命周期。
    pub fn start_flask(
        &self,
        port: u16,
        mirror: &MirrorConfig,
    ) -> Result<Child> {
        let app_py = self.app_dir.join("app.py");
        if !app_py.exists() {
            anyhow::bail!("app.py not found at {:?}", app_py);
        }

        log::info!("Starting Flask on port {}...", port);
        let mut cmd = Command::new(&self.venv_python);
        cmd.arg(&app_py)
            .arg("--port")
            .arg(port.to_string())
            .arg("--no-browser")
            .current_dir(&self.app_dir);

        // 设置 HuggingFace 镜像环境变量，加速模型下载
        if mirror.use_mirror {
            cmd.env("HF_ENDPOINT", &mirror.hf_endpoint);
        }

        // Unix: 创建独立进程组，退出时可整组 kill，避免子进程变孤儿
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let child = cmd.spawn().context("Failed to start Flask process")?;
        Ok(child)
    }

    /// 修复 OpenCV 包冲突。
    ///
    /// 问题背景：
    /// opencv-python 和 opencv-contrib-python 都提供 cv2 模块，
    /// 如果同时安装会互相覆盖文件导致不可预测的行为。
    ///
    /// 解决方法：
    /// 检测冲突 → 卸载非 contrib 版本 → 强制重装 opencv-contrib-python。
    /// contrib 版本包含所有 opencv-python 的功能加上扩展模块。
    fn fix_opencv_conflict(&self, uv_path: Option<&Path>) -> Result<()> {
        // 检查是否存在冲突的 opencv 包
        let output = Command::new(&self.venv_python)
            .args([
                "-c",
                "import importlib.metadata as m; \
                 names={'opencv-python','opencv-python-headless'}; \
                 found=[n for n in names if any(d.metadata['Name'].lower()==n \
                 for d in m.distributions())]; \
                 print('|'.join(found))",
            ])
            .output()?;

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let conflicts: Vec<&str> = stdout_str
            .trim()
            .split('|')
            .filter(|s| !s.is_empty())
            .collect();

        if conflicts.is_empty() {
            return Ok(());
        }

        log::info!("Fixing OpenCV conflict: {:?}", conflicts);

        // 卸载冲突的包（opencv-python 或 opencv-python-headless）
        if let Some(uv) = uv_path {
            let mut cmd = Command::new(uv);
            cmd.args(["pip", "uninstall", "-y", "--python"])
                .arg(&self.venv_python);
            for pkg in &conflicts {
                cmd.arg(pkg);
            }
            let _ = cmd.status();
        } else {
            for pkg in &conflicts {
                let _ = Command::new(&self.venv_python)
                    .args(["-m", "pip", "uninstall", "-y", pkg])
                    .status();
            }
        }

        // 重装 opencv-contrib-python（--force-reinstall --no-deps 跳过已满足的依赖，只重装本体）
        let status = if let Some(uv) = uv_path {
            Command::new(uv)
                .args(["pip", "install", "--python"])
                .arg(&self.venv_python)
                .args(["--force-reinstall", "--no-deps", "opencv-contrib-python>=4.9"])
                .status()?
        } else {
            Command::new(&self.venv_python)
                .args([
                    "-m", "pip", "install",
                    "--force-reinstall", "--no-deps",
                    "opencv-contrib-python>=4.9",
                ])
                .status()?
        };

        if !status.success() {
            anyhow::bail!("Failed to reinstall opencv-contrib-python");
        }
        Ok(())
    }
}

// ─── uv 发现 ───

/// 在系统中查找 uv 可执行文件。
///
/// uv 是一个用 Rust 编写的极速 Python 包管理器，
/// pip 的替代品，安装速度通常快 10-100 倍。
///
/// 查找顺序：
/// 1. PATH 中的 uv/uv.exe
/// 2. ~/.local/bin/uv（Linux/macOS pipx 默认安装路径）
/// 3. ~/.cargo/bin/uv（Rust cargo install 默认路径）
/// 4. Windows: %LOCALAPPDATA%\Programs\uv\uv.exe（官方安装器路径）
///
/// 返回 None 表示用户未安装 uv，后续操作回退到 pip。
fn find_uv() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "uv.exe"
    } else {
        "uv"
    };

    // 先检查 PATH（which/where）
    if let Some(p) = which_command(name) {
        return Some(p);
    }

    // PATH 中找不到，搜索常见安装路径
    let home = home_dir()?;

    #[cfg(target_os = "windows")]
    {
        let paths = [
            home.join(".local").join("bin").join(name),
            home.join(".cargo").join("bin").join(name),
            // uv 官方安装器在 Windows 上的默认路径
            home.join("AppData").join("Local").join("Programs").join("uv").join(name),
        ];
        for p in &paths {
            if p.exists() {
                return Some(p.clone());
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let paths = [
            home.join(".local").join("bin").join(name),
            home.join(".cargo").join("bin").join(name),
        ];
        for p in &paths {
            if p.exists() {
                return Some(p.clone());
            }
        }
    }

    None
}

/// 确保 uv 可用 — 找不到时自动下载安装。
///
/// 与原始启动脚本行为一致：
/// - macOS/Linux: curl -LSf https://astral.sh/uv/install.sh | sh
/// - Windows: powershell irm https://astral.sh/uv/install.ps1 | iex
///
/// 安装后重新搜索，仍找不到则返回 None（网络问题等）。
fn ensure_uv(on_progress: impl Fn(&str)) -> Option<PathBuf> {
    if let Some(uv) = find_uv() {
        return Some(uv);
    }

    log::info!("uv not found, attempting auto-install...");
    on_progress("正在下载安装 uv（约需 30 秒）...");

    let installed = if cfg!(target_os = "windows") {
        match std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "irm https://astral.sh/uv/install.ps1 | iex",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() {
                    for line in BufReader::new(stderr).lines().flatten() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            on_progress(trimmed);
                        }
                    }
                }
                if let Some(stdout) = child.stdout.take() {
                    for _ in BufReader::new(stdout).lines() {}
                }
                child.wait().map(|s| s.success()).unwrap_or(false)
            }
            Err(_) => false,
        }
    } else {
        match std::process::Command::new("sh")
            .arg("-c")
            .arg("curl -LSf https://astral.sh/uv/install.sh | sh")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() {
                    for line in BufReader::new(stderr).lines().flatten() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            on_progress(trimmed);
                        }
                    }
                }
                child.wait().map(|s| s.success()).unwrap_or(false)
            }
            Err(_) => false,
        }
    };

    if installed {
        on_progress("uv 安装完成");
        log::info!("uv install script completed, re-scanning...");
        find_uv()
    } else {
        log::warn!("uv auto-install failed or curl not available");
        None
    }
}

/// 跨平台的命令查找（类似 Unix which / Windows where）。
fn which_command(name: &str) -> Option<PathBuf> {
    let (check_cmd, check_arg) = if cfg!(target_os = "windows") {
        ("where", name)
    } else {
        ("which", name)
    };

    Command::new(check_cmd)
        .arg(check_arg)
        .output()
        .ok()
        .and_then(|o| {
            let path_str = String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if path_str.is_empty() {
                return None;
            }
            let path = PathBuf::from(&path_str);
            if path.exists() {
                Some(path)
            } else {
                None
            }
        })
}

/// 跨平台获取用户主目录。
fn home_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

// ─── 虚拟环境创建 ───

/// 创建 Python 虚拟环境。
///
/// 策略：
/// 1. 优先使用 uv venv — 不需要系统 Python，uv 可自动下载合适版本
/// 2. uv 不可用或失败时，回退到系统 Python 的标准库 venv 模块
/// 3. 两者都不可用则返回错误
///
/// uv venv --python ">=3.10" 的含义：
/// - 如果系统已有 Python 3.10+，直接使用
/// - 如果没有，uv 自动从 python-build-standalone 下载预编译的 Python
fn create_venv(python_path: Option<&Path>, venv_dir: &Path, use_ensure_uv: bool, on_progress: impl Fn(&str)) -> Result<()> {
    let uv = if use_ensure_uv { ensure_uv(&on_progress) } else { find_uv() };
    if let Some(ref uv) = uv {
        on_progress("使用 uv 创建虚拟环境（可能需下载 Python）...");
        log::info!("Using uv to create venv (may auto-download Python)...");
        // uv venv --python ">=3.10" 自动选择/下载符合要求的 Python 版本
        let mut child = Command::new(uv)
            .args(["venv", "--python", ">=3.10"])
            .arg(venv_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to run uv venv")?;

        // 流式读取 stderr 以显示进度（uv 可能需下载 Python 解释器）
        if let Some(stderr) = child.stderr.take() {
            for line in BufReader::new(stderr).lines().flatten() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    on_progress(trimmed);
                }
            }
        }
        // 排空 stdout
        if let Some(stdout) = child.stdout.take() {
            for _ in BufReader::new(stdout).lines() {}
        }

        let status = child.wait()?;
        if status.success() {
            log::info!("uv venv created successfully");
            let venv_python = if cfg!(target_os = "windows") {
                venv_dir.join("Scripts").join("python.exe")
            } else {
                venv_dir.join("bin").join("python3")
            };
            if venv_python.exists() {
                return Ok(());
            }
            log::warn!("uv venv completed but python binary not found, trying fallback...");
            on_progress("uv 创建的 venv 不完整，尝试系统 Python...");
        } else {
            log::warn!("uv venv failed");
            on_progress("uv 创建 venv 失败，回退到系统 Python...");
        }
    }

    // 回退：使用系统 Python 的标准库 venv
    let python_path = python_path
        .ok_or_else(|| anyhow::anyhow!("未找到系统 Python 且 uv 不可用，请安装 Python 3.10+ 或 uv"))?;
    log::info!("Falling back to system Python + venv...");
    on_progress("使用系统 Python 创建虚拟环境...");
    let status = Command::new(python_path)
        .args(["-m", "venv"])
        .arg(venv_dir)
        .status()
        .context("Failed to create virtual environment")?;

    if !status.success() {
        anyhow::bail!("venv creation failed (both uv and system Python)");
    }
    Ok(())
}

// ─── 资源提取 ───

/// 解析资源的实际来源目录。
///
/// Tauri 打包后，资源文件在 .app/Contents/Resources/_up_/ 目录下
/// （_up_ 是 Tauri 的内部目录结构）。
/// 开发模式下资源直接在 resource_dir 中，没有 _up_ 子目录。
fn resolve_resource_src(resource_dir: &Path) -> PathBuf {
    let up_dir = resource_dir.join("_up_");
    if up_dir.exists() && up_dir.is_dir() {
        return up_dir;
    }
    resource_dir.to_path_buf()
}

/// 将资源目录的内容复制到目标目录。
///
/// 跳过 _up_ 目录（Tauri 内部）和图标文件（不需要在运行时访问）。
/// 如果资源目录中没有 app.py（可能处于开发模式），
/// 则回退到从项目根目录复制。
fn copy_resource_dir(src: &Path, dst: &Path) -> Result<()> {
    let actual_src = resolve_resource_src(src);
    let has_app_py = actual_src.join("app.py").exists();

    if !has_app_py {
        // 开发模式：资源目录中没有 app.py，从项目根目录复制
        log::warn!("No bundled resources, falling back to project root (dev mode?)");
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        return copy_project_root(project_root, dst);
    }

    log::info!("Copying resources from {:?}", actual_src);
    for entry in std::fs::read_dir(&actual_src)? {
        let entry = entry?;
        let name = entry.file_name();
        // 跳过 Tauri 内部目录和图标文件
        if name == "_up_" || name == "icon.icns" || name == "icon.ico" {
            continue;
        }
        let dst_path = dst.join(&name);
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// 递归复制目录内容。
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// 从项目根目录复制关键文件（开发模式回退方案）。
///
/// 只复制 Flask 应用需要的文件和目录。
fn copy_project_root(project_root: &Path, dst: &Path) -> Result<()> {
    for name in &["app.py", "requirements.txt", "pic_selecter", "static", "assets"] {
        let src = project_root.join(name);
        if !src.exists() {
            continue;
        }
        let dst_path = dst.join(name);
        if src.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src, &dst_path)?;
        } else {
            std::fs::copy(&src, &dst_path)?;
        }
    }
    Ok(())
}
