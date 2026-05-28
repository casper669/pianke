//! 应用更新模块 — 从 GitHub 检查并下载代码更新。
//!
//! 更新策略：
//! 1. 通过 GitHub API 获取 main 分支的最新 commit SHA
//! 2. 与本地保存的 SHA 比较，不同则下载新版本
//! 3. 解压 tarball 到 app_dir，同时保护用户数据不被覆盖
//! 4. 首次启动只记录 SHA，不强制更新
//!
//! 保护的用户数据（更新时跳过）：
//! - .venv — 虚拟环境，不应被更新覆盖
//! - 安装状态文件和依赖戳记
//! - 用户生成的图片、日志等
//!
//! HTTP 请求与 tar 解压通过 Python 标准库（urllib / tarfile）完成，
//! 不依赖外部命令（curl / tar），与原版 launcher.py 保持一致。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::launcher;

/// GitHub 仓库信息
const GITHUB_OWNER: &str = "zhaoyue4810";
const GITHUB_REPO: &str = "pianke";
const GITHUB_BRANCH: &str = "main";

/// 更新时保护的文件/目录列表（不会被新版本覆盖）。
///
/// 支持两种匹配模式：
/// - 精确匹配：名称完全相等（如 ".venv"）
/// - 通配符后缀：*.session.json 匹配所有 .session.json 结尾的文件
const PRESERVE: &[&str] = &[
    ".venv",                        // 虚拟环境，包含已安装的 Python 包
    ".pic_selecter_install.json",   // 安装状态（模式选择、包签名）
    ".pic_selecter_deps.stamp",     // 依赖安装完成的戳记文件
    "__pycache__",                  // Python 字节码缓存
    ".git",                         // Git 仓库数据
    ".cache",                       // HuggingFace/transformers 缓存
    "state.json",                   // 用户配置状态
    "*.session.json",               // Flask session 文件
    "img",                          // 用户图片目录
    "log.txt",                      // 运行日志
    ".DS_Store",                    // macOS 目录元数据
];

/// 从 GitHub API 获取 main 分支的最新 commit SHA。
///
/// 使用 Python urllib（标准库）发起 HTTPS 请求，不依赖 curl。
/// Python 由调用方传入（venv Python 或系统 Python）。
///
/// 返回 None 表示网络错误或 API 限流，不阻塞正常启动。
pub fn fetch_remote_sha(python: &Path) -> Result<Option<String>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        GITHUB_OWNER, GITHUB_REPO, GITHUB_BRANCH
    );

    log::info!("Checking for updates: {}", url);

    // Python 脚本：用 urllib 请求 GitHub API，解析 commit SHA
    // URL 通过 sys.argv[1] 传入，避免字符串转义问题
    let script = "import urllib.request,json,sys\n\
try:\n req=urllib.request.Request(sys.argv[1],headers={'User-Agent':'pianke-updater'})\n data=json.loads(urllib.request.urlopen(req,timeout=10).read())\n print(data.get('sha',''))\n\
except Exception as e:\n print('__ERROR__:',e)";

    let output = Command::new(python)
        .args(["-c", script, &url])
        .output()
        .context("Failed to run Python for update check")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let sha = stdout.trim();

    if sha.is_empty() || sha.starts_with("__ERROR__") {
        if sha.starts_with("__ERROR__") {
            log::warn!("GitHub API error: {}", sha);
        }
        return Ok(None);
    }

    Ok(Some(sha.to_string()))
}

/// 从 GitHub 下载指定 commit 的源代码 tarball。
///
/// 使用 Python urllib 下载，不依赖 curl。
/// 超时设置为 120 秒，专家模式的代码库可能较大。
pub fn download_tarball(sha: &str, dest: &Path, python: &Path) -> Result<()> {
    let url = format!(
        "https://codeload.github.com/{}/{}/tar.gz/{}",
        GITHUB_OWNER, GITHUB_REPO, sha
    );

    log::info!("Downloading update from: {}", url);

    // Python 脚本：用 urllib 下载文件到指定路径
    // sys.argv[1] = dest 路径, sys.argv[2] = URL（避免字符串转义问题）
    let script = "import sys,urllib.request\n\
req=urllib.request.Request(sys.argv[2],headers={'User-Agent':'pianke-updater'})\n\
data=urllib.request.urlopen(req,timeout=120).read()\n\
open(sys.argv[1],'wb').write(data)\n\
print('ok')";

    let status = Command::new(python)
        .args(["-c", script, dest.to_str().unwrap_or(""), &url])
        .status()
        .context("Failed to download tarball via Python")?;

    if !status.success() {
        anyhow::bail!("Download failed with status: {}", status);
    }

    Ok(())
}

/// 解压 tarball 到 app_dir，保护用户数据不被覆盖。
///
/// 过程：
/// 1. 用 Python tarfile 解压到临时目录 .update_tmp/
/// 2. tarball 结构：pianke-<sha>/  → 一个顶层目录包含所有文件
/// 3. 遍历顶层目录的内容，跳过 PRESERVE 列表中的文件
/// 4. 将剩余内容复制到 app_dir，覆盖旧文件
/// 5. 清理临时目录和 tarball
///
/// GitHub 的 codeload tarball 内部目录名为 {repo}-{sha}，
/// 所以需要取唯一的子目录作为源。
pub fn extract_tarball(tar_path: &Path, app_dir: &Path, python: &Path) -> Result<()> {
    // 在 app_dir 同级创建临时目录，避免与 app_dir 内的文件混淆
    let tmp_dir = app_dir.with_file_name(".update_tmp");

    // 清理之前可能失败的提取残留
    if tmp_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    std::fs::create_dir_all(&tmp_dir)?;

    log::info!("Extracting update to {:?}", tmp_dir);

    // 用 Python tarfile 解压（标准库，不依赖外部 tar 命令）
    let script = "import sys,tarfile\n\
tarfile.open(sys.argv[1],'r:gz').extractall(sys.argv[2])\n\
print('ok')";

    let status = Command::new(python)
        .args([
            "-c", &script,
            tar_path.to_str().unwrap_or(""),
            tmp_dir.to_str().unwrap_or(""),
        ])
        .status()
        .context("Failed to extract tarball via Python")?;

    if !status.success() {
        anyhow::bail!("tar extraction failed");
    }

    // 查找 tarball 中的顶层目录（如 pianke-abc123def/）
    let children: Vec<PathBuf> = std::fs::read_dir(&tmp_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    if children.len() != 1 {
        anyhow::bail!("Unexpected tarball structure: expected 1 top-level directory");
    }

    let src = &children[0];

    // 遍历新代码，跳过受保护的文件
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if should_preserve(&name_str) {
            log::debug!("Preserving: {}", name_str);
            continue;
        }

        let target = app_dir.join(&name);

        // 先删除旧文件/目录再复制（处理目录变文件或反向的情况）
        if target.exists() {
            if target.is_dir() {
                let _ = std::fs::remove_dir_all(&target);
            } else {
                let _ = std::fs::remove_file(&target);
            }
        }

        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }

    // 清理：删除临时目录和下载的 tarball
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_file(tar_path);

    log::info!("Update extracted successfully");
    Ok(())
}

/// 判断文件或目录是否应在更新时保留。
///
/// 匹配规则：
/// - 精确匹配：name == pattern
/// - 通配符后缀：pattern 以 "*." 开头时，检查 name 是否以对应后缀结尾
fn should_preserve(name: &str) -> bool {
    for pattern in PRESERVE {
        if pattern.starts_with("*.") {
            // 通配符匹配：*.session.json → 匹配任何 .session.json 文件
            let suffix = &pattern[1..]; // 去掉 *，得到 .session.json
            if name.ends_with(suffix) {
                return true;
            }
        } else if *pattern == name {
            return true;
        }
    }
    false
}

/// 检查更新并应用（同步操作，约 2-5 秒）。
///
/// 完整流程：
/// 1. 从 GitHub 获取最新 SHA
/// 2. 首次启动：只记录当前 SHA，不下载（用户刚安装就是最新的）
/// 3. SHA 不同：下载 tarball → 解压 → 更新状态文件
/// 4. 清空 packages_sig 以强制下次启动时重检依赖
///
/// `python` 用于 HTTP 请求和 tar 解压（使用 Python 标准库，避免外部命令依赖）。
///
/// 返回 Some(sha) 表示完成了一次更新，None 表示无更新或检查失败。
pub fn check_and_apply(
    app_dir: &Path,
    state_path: &Path,
    python: &Path,
    on_progress: &mut impl FnMut(&str),
) -> Option<String> {
    let state = launcher::load_install_state(state_path);
    let local_sha = state.commit_sha.clone();

    // 获取远程最新 SHA
    on_progress("正在检查更新...");
    let remote_sha = match fetch_remote_sha(python) {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            on_progress("检查更新失败（网络问题），跳过");
            return None;
        }
        Err(e) => {
            log::warn!("Update check failed: {}", e);
            on_progress(&format!("检查更新失败，跳过: {}", e));
            return None;
        }
    };

    // 首次启动：不强制更新，只记录当前线上 SHA 作为基准
    if local_sha.is_none() {
        log::info!("First launch, recording SHA: {}", &remote_sha[..8.min(remote_sha.len())]);
        let state = launcher::InstallState {
            commit_sha: Some(remote_sha),
            ..state
        };
        launcher::save_install_state(state_path, &state);
        return None;
    }

    // 比较 SHA，相同则无需更新
    let local = local_sha.as_deref().unwrap_or("");
    if local == remote_sha {
        on_progress("已是最新版本");
        return None;
    }

    // 发现新版本，开始更新
    on_progress(&format!(
        "发现新版本 {}，正在更新...",
        &remote_sha[..8.min(remote_sha.len())]  // 只显示 SHA 前 8 位
    ));

    // 下载 tarball 到 app_dir 同级的临时文件
    let tar_path = app_dir.with_file_name(".update.tar.gz");
    if let Err(e) = download_tarball(&remote_sha, &tar_path, python) {
        log::warn!("Download failed: {}", e);
        on_progress(&format!("下载失败: {}", e));
        return None;
    }

    on_progress("正在应用更新...");

    // 解压到 app_dir，保护用户数据
    if let Err(e) = extract_tarball(&tar_path, app_dir, python) {
        log::warn!("Extraction failed: {}", e);
        on_progress(&format!("更新失败: {}", e));
        return None;
    }

    // 更新状态：
    // - 记录新 SHA 以便下次比较
    // - 清空 packages_sig 强制下次启动时检查依赖是否匹配
    //   （代码更新后可能增减了依赖）
    let new_state = launcher::InstallState {
        commit_sha: Some(remote_sha.clone()),
        packages_sig: String::new(),
        modes: state.modes,
    };
    launcher::save_install_state(state_path, &new_state);

    on_progress("代码已更新，下次启动生效");
    log::info!("Updated to {}", &remote_sha[..8.min(remote_sha.len())]);

    Some(remote_sha)
}

/// 递归复制目录（用于更新时覆盖旧文件）。
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
