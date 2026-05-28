//! 启动器 — 模式定义、镜像配置、安装状态持久化、应用全局状态。
//!
//! 三种功能模式：
//! - 极速模式 (fast)：纯本地视觉算法，轻量快速
//! - 专家模式 (expert)：深度学习模型，效果最好但依赖大
//! - 土豪模式 (tycoon)：调用远程大模型 API，无需本地 GPU
//!
//! 模式之间可以叠加选择（如同时选极速 + 土豪），包列表会自动去重合并。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// 安装状态文件的文件名，保存在 app_data_dir 下
pub const STATE_FILE: &str = ".pic_selecter_install.json";

/// 返回安装状态文件的完整路径
pub fn state_file_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(STATE_FILE)
}

// ─── 包定义 ───
// 每个模式对应一组 pip 包，最终按用户选择的模式合并去重

/// 核心包 — 所有模式都需要的基础依赖
pub const CORE_PACKAGES: &[&str] = &[
    "Pillow>=10.0",              // 图像处理基础库
    "pillow-heif>=0.16",         // HEIC/HEIF 格式支持（iPhone 照片）
    "numpy>=1.26",               // 数值计算
    "scipy>=1.11",               // 科学计算
    "flask>=3.0",                // Web 服务
    "imagehash>=4.3",            // 感知哈希（找相似图）
    "opencv-contrib-python>=4.9", // 计算机视觉（含 contrib 扩展模块）
    "rawpy>=0.18",               // RAW 格式支持（相机原片）
    "piexif>=1.1.3",             // EXIF 信息处理
];

/// 专家模式额外包 — 深度学习模型推理
pub const EXPERT_PACKAGES: &[&str] = &[
    "torch>=2.2",           // PyTorch 深度学习框架
    "torchvision>=0.17",    // 计算机视觉模型
    "transformers>=4.40",   // HuggingFace transformers（DINOv2 等）
    "insightface>=0.7",     // 人脸识别
    "onnxruntime>=1.16",    // ONNX 模型推理
    "pyiqa>=0.1.10",        // 图像质量评估
    "timm>=0.9",            // PyTorch 图像模型库
];

/// 土豪模式额外包 — 云端 API 调用
pub const TYCOON_PACKAGES: &[&str] = &["openai>=1.40"];

// ─── 模式信息 ───

/// 前端展示的模式卡片数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeInfo {
    /// 模式标识符，与后端逻辑对应
    pub key: String,
    /// 中文显示名称
    pub label: String,
    /// 功能描述
    pub description: String,
    /// 预估下载大小
    pub download_size: String,
    /// 预估安装时间
    pub time_estimate: String,
    /// 是否默认勾选（从上次安装状态恢复）
    pub selected: bool,
}

/// 构建模式列表，根据上次安装的模式标记 selected。
///
/// 规则：
/// - 如果 previous 为空（首次安装），默认勾选"极速模式"
/// - 否则恢复上次的勾选状态
/// - 极速模式：如果上次选了 expert 或 tycoon，且包签名没变，
///   上次没选 fast 但这里不影响 — selected 只看 previous 列表
pub fn get_modes(previous: &[String]) -> Vec<ModeInfo> {
    // 首次安装默认开启极速模式
    let default_on = previous.is_empty()
        || previous.iter().any(|m| m == "fast");

    vec![
        ModeInfo {
            key: "fast".into(),
            label: "极速模式".into(),
            description: "纯本地视觉算法（哈希 + 拉普拉斯 + ORB），约 200MB 依赖".into(),
            download_size: "~200MB".into(),
            time_estimate: "1-3 分钟".into(),
            selected: default_on,
        },
        ModeInfo {
            key: "expert".into(),
            label: "专家模式".into(),
            description: "深度学习（DINOv2 + InsightFace + 图像质量评估），约 2-3GB 依赖".into(),
            download_size: "~2-3GB".into(),
            time_estimate: "5-15 分钟".into(),
            selected: previous.iter().any(|m| m == "expert"),
        },
        ModeInfo {
            key: "tycoon".into(),
            label: "土豪模式".into(),
            description: "调用远程大模型判图（火山方舟 API），需自备 API key".into(),
            download_size: "~5MB".into(),
            time_estimate: "约 30 秒".into(),
            selected: previous.iter().any(|m| m == "tycoon"),
        },
    ]
}

/// 根据所选模式构建完整包列表，自动去重。
///
/// 去重策略：按包名（不含版本约束）做小写比较，
/// 同一个包只保留第一次出现的版本约束。
/// 使用 BTreeSet 保证输出有序，确保签名稳定。
pub fn packages_for_modes(modes: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut result = Vec::new();

    // 核心包始终包含
    for pkg in CORE_PACKAGES {
        let name = pkg_name(pkg);
        if seen.insert(name) {
            result.push(pkg.to_string());
        }
    }

    // 按模式追加额外包
    for mode in modes {
        let packages: &[&str] = match mode.as_str() {
            "expert" => EXPERT_PACKAGES,
            "tycoon" => TYCOON_PACKAGES,
            _ => &[],
        };
        for pkg in packages {
            let name = pkg_name(pkg);
            if seen.insert(name) {
                result.push(pkg.to_string());
            }
        }
    }

    result.sort();
    result
}

/// 从 pip 包约束字符串中提取包名（不含版本号）。
///
/// 例如 "opencv-contrib-python>=4.9" → "opencv-contrib-python"
fn pkg_name(spec: &str) -> String {
    spec.split(&['=', '>', '<', '!', '~', ';', '['][..])
        .next()
        .unwrap_or(spec)
        .to_lowercase()
}

// ─── 镜像配置 ───

/// pip 和 HuggingFace 的镜像源配置。
///
/// 国内用户默认使用清华大学 TUNA 镜像加速下载，
/// 可通过环境变量 PIANKE_NO_MIRROR=1 关闭镜像，使用官方源。
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// 是否启用镜像加速
    pub use_mirror: bool,
    /// pip 主镜像源地址
    pub pypi_index: String,
    /// pip 备用源地址（主源找不到时回退）
    pub pypi_extra: String,
    /// 镜像源的中文标签（前端展示用）
    pub pypi_label: String,
    /// HuggingFace 模型下载镜像
    pub hf_endpoint: String,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        // PIANKE_NO_MIRROR=1 可强制使用官方源（海外用户或镜像故障时）
        let use_mirror =
            std::env::var("PIANKE_NO_MIRROR").map_or(true, |v| v != "1");
        Self {
            use_mirror,
            pypi_index: "https://pypi.tuna.tsinghua.edu.cn/simple/".into(),
            pypi_extra: "https://pypi.org/simple/".into(),
            pypi_label: "清华大学 TUNA".into(),
            hf_endpoint: "https://hf-mirror.com".into(),
        }
    }
}

// ─── 安装状态持久化 ───

/// 保存在 app_data_dir 中的安装状态，JSON 格式。
///
/// 作用：
/// - 记录上次选择的模式，下次启动可恢复
/// - 记录包签名，用于判断是否需要重装依赖
/// - 记录 commit SHA，用于更新检测
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    /// 已安装的模式列表
    #[serde(default)]
    pub modes: Vec<String>,
    /// 包签名 = packages.join("|")，签名变了就触发重装
    #[serde(default)]
    pub packages_sig: String,
    /// 已安装代码的 GitHub commit SHA，用于增量更新检测
    #[serde(default)]
    pub commit_sha: Option<String>,
}

/// 从 JSON 文件加载安装状态，文件不存在时返回空状态。
pub fn load_install_state(path: &Path) -> InstallState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| InstallState {
            modes: vec![],
            packages_sig: String::new(),
            commit_sha: None,
        })
}

/// 将安装状态序列化为 JSON 并写入文件。
pub fn save_install_state(path: &Path, state: &InstallState) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, json);
    }
}

// ─── 应用全局状态 ───

/// Tauri 管理的全局状态，在 setup 阶段创建，各命令通过 State<'_, AppState> 访问。
///
/// venv_python 使用 Mutex 包裹是因为：
/// - setup 阶段可能无法创建 venv（无 Python 且无 uv）
/// - init_setup 命令可以在运行时重试创建 venv 并更新该字段
/// - 多线程安全：后台安装线程读取，窗口关闭线程写入
#[allow(dead_code)]
pub struct AppState {
    /// 打包资源的目录（.app/Contents/Resources/）
    pub resource_dir: PathBuf,
    /// 应用数据目录（~/Library/Application Support/com.pianke.desktop/）
    pub app_data_dir: PathBuf,
    /// 系统 Python 的路径（用于创建 venv）
    pub python_path: PathBuf,
    /// venv 中的 Python 可执行文件路径（Mutex 允许运行时更新）
    pub venv_python: Mutex<PathBuf>,
    /// 代码副本目录（app_data_dir/app/）
    pub app_dir: PathBuf,
    /// Tauri 前端页面的初始 URL，用于从 Flask 页面切回前端
    pub home_url: String,
}
