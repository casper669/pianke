"""扫描文件夹，计算 pHash + 提取 EXIF 摘要，按视觉相似度分组。

分组原则：pHash 汉明距离为主，EXIF 时间为辅助约束（时间近 → 阈值更宽松）。
"""

from __future__ import annotations

import io
import os
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, Optional

import imagehash
import numpy as np
from PIL import Image, ImageOps
from pic_selecter.quality import analyze_image

try:
    from pillow_heif import register_heif_opener
    register_heif_opener()
except Exception:
    pass

# PIL 直接能解码的格式
IMAGE_EXTS = {".jpg", ".jpeg", ".png", ".heic", ".heif", ".webp", ".bmp", ".tif", ".tiff"}

# RAW 格式：靠 rawpy 提取内嵌 JPEG 预览图来分析，原文件搬运时整个搬。
# 如果同 stem 同目录有 JPG/JPEG 等 IMAGE_EXTS 文件，则优先用那个（更高质量，不需要 rawpy）。
RAW_EXTS = {
    ".cr2", ".cr3", ".crw",        # Canon
    ".nef", ".nrw",                # Nikon
    ".arw", ".srf", ".sr2",        # Sony
    ".dng",                        # Adobe / 通用
    ".raf",                        # Fuji
    ".orf",                        # Olympus
    ".rw2",                        # Panasonic
    ".pef",                        # Pentax
    ".rwl",                        # Leica
    ".srw",                        # Samsung
    ".x3f",                        # Sigma
}

ALL_INPUT_EXTS = IMAGE_EXTS | RAW_EXTS

# 分析尺寸：所有 AI 模型 / 质量算法吃的最大长边。
# 相机原图 5152×7728 = 40MP → 直接喂 pyiqa CLIP 会 MPS OOM（34 GiB buffer）。
# 2048 长边 ≈ 4.2MP 够所有模型用、对小脸保留检测可能：
#   - DINOv2/NIMA：输入 224×224，自带 resize
#   - InsightFace SCRFD：det_size=640；原图 100px 小脸缩到 2048 仍有 ~40px → 1024 内部 20px →
#     640 内部 ~12-13px；2048 是单脸正常拍摄场景的安全下限
#   - MUSIQ：训练在多尺度（384+），2048 已远超
#   - CLIP-IQA+：CLIP backbone 224×224；pyiqa 自身又会缩到 1024
#   - Laplacian/saliency：质量算法内部进一步降到 768
# 原图磁盘不动；winners/losers 物理搬运用原文件；浏览器显示直接读原图。
ANALYSIS_MAX_SIDE = 2048


def _resize_for_analysis(img: Image.Image) -> Image.Image:
    """统一压缩到 ANALYSIS_MAX_SIDE 长边——所有模型分析吃这个。

    返回 RGB 模式 PIL Image。已经够小 → 直接返回（仍转 RGB 保证下游一致）。
    """
    w, h = img.size
    if max(w, h) <= ANALYSIS_MAX_SIDE:
        return img if img.mode == "RGB" else img.convert("RGB")
    scale = ANALYSIS_MAX_SIDE / max(w, h)
    new_w = max(1, int(w * scale))
    new_h = max(1, int(h * scale))
    return img.convert("RGB").resize((new_w, new_h), Image.LANCZOS)


# 阈值：时间间隔近时（同一拍摄场景），允许更大的视觉差异
THRESHOLD_NEAR = 10   # 同场景内 (<= 5 分钟)
THRESHOLD_FAR = 6     # 跨场景 (> 5 分钟)
NEAR_SECONDS = 300    # 5 分钟


class CancelledError(Exception):
    """compute_infos 被外部取消时抛出。"""


@dataclass
class ImageInfo:
    path: str                               # primary 文件路径（可能是 RAW 或普通图片）
    phash: str                              # 64 位 hex
    timestamp: Optional[float] = None       # unix 秒；naive datetime → ts，仅用于秒差比较
    size: int = 0
    mtime: float = 0.0
    exif_summary: Optional[dict] = None     # 展示用 EXIF 摘要
    quality: Optional[dict] = None          # 技术质量评分与初筛信号
    # 同 stem 同目录的伴随文件（与 primary 一起搬运，不单独参与分析）
    # 例如 primary = IMG_001.CR2，companions = ["IMG_001.JPG"]
    companions: list[str] = field(default_factory=list)
    # ---- 视觉模型产物（专家模式 / vision.py 生成）----
    dinov2: Optional[Any] = None            # 384 维 float32 np.ndarray，L2 归一
    aesthetic_score: Optional[float] = None # 1-10 美学分（NIMA）
    musiq_score: Optional[float] = None     # 0-100 技术质量（pyiqa MUSIQ）
    clipiqa_score: Optional[float] = None   # 0-1 LAION 美学（pyiqa CLIP-IQA+）
    face_embeddings: Optional[list] = None  # [512 维 np.ndarray, ...]，InsightFace ArcFace
    # ---- 土豪模式（tycoon）专属 ----
    llm_verdict: Optional[str] = None       # "pass" | "reject"
    llm_reason: Optional[str] = None        # 一句中文短理由
    # ---- 极速模式签名（fast_clustering 消费）----
    dhash: Optional[str] = None             # 64 位 hex
    whash: Optional[str] = None             # 64 位 hex
    ahash: Optional[str] = None             # 64 位 hex
    color_hist: Optional[Any] = None        # 144 维 float32（HSV 3×3 块 × 16 bins）
    orb_descs: Optional[Any] = None         # (N, 32) uint8 ORB 描述子
    orb_kps: Optional[Any] = None           # (N, 2) float32 关键点坐标


# ---------------- EXIF ----------------

def _parse_exif_datetime(dt_str: str, subsec: str = "0") -> Optional[datetime]:
    if not dt_str:
        return None
    try:
        dt = datetime.strptime(dt_str.strip(), "%Y:%m:%d %H:%M:%S")
    except (ValueError, AttributeError):
        return None
    try:
        frac = float("0." + str(subsec).strip())
    except ValueError:
        frac = 0.0
    return dt.replace(microsecond=int(frac * 1_000_000))


def _read_exif_datetime(img: Image.Image) -> Optional[datetime]:
    """优先 DateTimeOriginal + SubSecTimeOriginal。返回 naive datetime。"""
    try:
        exif = img.getexif()
        if not exif:
            return None
        ifd = exif.get_ifd(0x8769) if 0x8769 in exif else {}
        dt_str = ifd.get(0x9003) or exif.get(0x9003) or exif.get(306)
        subsec = ifd.get(0x9291) or "0"
        return _parse_exif_datetime(str(dt_str) if dt_str else "", subsec)
    except Exception:
        return None


def _format_shutter(exposure: float) -> str:
    if exposure <= 0:
        return ""
    if exposure >= 1:
        return f"{exposure:g}s"
    denom = round(1.0 / exposure)
    return f"1/{denom}s"


def extract_exif_summary(img: Image.Image, file_size: int) -> dict:
    """提取展示用的 EXIF 摘要。所有字段缺失时返回最小集（width/height/file_size）。"""
    out: dict = {
        "width": img.width,
        "height": img.height,
        "file_size": file_size,
    }
    try:
        exif = img.getexif()
    except Exception:
        return out
    if not exif:
        return out

    ifd = exif.get_ifd(0x8769) if 0x8769 in exif else {}

    def _to_float(val):
        if val is None:
            return None
        try:
            return float(val)
        except (TypeError, ValueError):
            return None

    make = str(exif.get(0x010F) or "").strip()
    model = str(exif.get(0x0110) or "").strip()
    if model and make and model.lower().startswith(make.lower()):
        camera = model
    elif make and model:
        camera = f"{make} {model}"
    else:
        camera = make or model or None
    if camera:
        out["camera"] = camera

    lens = ifd.get(0xA434) or ifd.get(0xFDEA)
    if lens:
        s = str(lens).strip()
        if s:
            out["lens"] = s

    aperture = _to_float(ifd.get(0x829D))
    if aperture:
        out["aperture"] = f"f/{aperture:g}"

    exposure = _to_float(ifd.get(0x829A))
    if exposure:
        out["shutter"] = _format_shutter(exposure)

    iso = ifd.get(0x8827)
    if isinstance(iso, (list, tuple)):
        iso = iso[0] if iso else None
    if iso:
        out["iso"] = str(iso)

    fl = _to_float(ifd.get(0x920A))
    if fl:
        out["focal_length"] = f"{fl:g}mm"

    dt = _read_exif_datetime(img)
    if dt:
        out["datetime"] = dt.isoformat()

    return out


# ---------------- 极速模式辅助：HSV 直方图 + ORB ----------------
# cv2 是极速模式硬依赖；缺失就让本模块导入失败、整次任务挂掉（不静默降级）。
# 但极速模式跑的时候不强制 import cv2，专家模式不需要；只有 _compute_*
# 函数会用到时才会触发顶层 import。这里把 import 提到顶部，使能缺失
# 不会等到"每张图都失败"才暴露。

def _ensure_cv2():
    """fast 模式硬依赖检查；缺失抛 ImportError 让上游 _run_job 接住报错。"""
    import cv2  # noqa: F401
    return cv2


def _compute_color_hist(img_t: Image.Image) -> Optional[np.ndarray]:
    """HSV 3×3 分块直方图（每块 H/S/V 各 16 bins，拼成 144 维）。L2 归一。

    cv2 缺失 → ImportError 透传（启动期已 prewarm 过，正常情况不会到这里）。
    图过小或退化 → 返回 None（数据不足，由 fast_clustering 的 _color_sim 接住）。
    """
    cv2 = _ensure_cv2()
    rgb = np.asarray(img_t.convert("RGB"))
    h, w = rgb.shape[:2]
    if h < 16 or w < 16:
        return None
    if max(h, w) > 384:
        scale = 384.0 / max(h, w)
        rgb = cv2.resize(rgb, (max(1, int(w * scale)), max(1, int(h * scale))),
                         interpolation=cv2.INTER_AREA)
        h, w = rgb.shape[:2]
    hsv = cv2.cvtColor(rgb, cv2.COLOR_RGB2HSV)
    feats: list[np.ndarray] = []
    ys = [0, h // 3, 2 * h // 3, h]
    xs = [0, w // 3, 2 * w // 3, w]
    for i in range(3):
        for j in range(3):
            block = hsv[ys[i]:ys[i + 1], xs[j]:xs[j + 1]]
            hist_h = cv2.calcHist([block], [0], None, [16], [0, 180]).flatten()
            hist_s = cv2.calcHist([block], [1], None, [16], [0, 256]).flatten()
            hist_v = cv2.calcHist([block], [2], None, [16], [0, 256]).flatten()
            for arr in (hist_h, hist_s, hist_v):
                s = arr.sum()
                if s > 0:
                    arr /= s
            feats.extend([hist_h, hist_s, hist_v])
    vec = np.concatenate(feats).astype(np.float32)
    n = float(np.linalg.norm(vec))
    if n < 1e-8:
        return None
    return vec / n


def _compute_orb(img_t: Image.Image, nfeatures: int = 500):
    """提取 ORB 关键点 + 描述子。返回 (descs (N,32) uint8, kps (N,2) float32) 或 (None, None)。

    cv2 缺失 → ImportError 透传。图过小 / 找不到足够关键点 → 返回 (None, None)
    （数据不足）。
    """
    cv2 = _ensure_cv2()
    gray = np.asarray(img_t.convert("L"))
    h, w = gray.shape[:2]
    if h < 32 or w < 32:
        return None, None
    if max(h, w) > 800:
        scale = 800.0 / max(h, w)
        gray = cv2.resize(gray, (max(1, int(w * scale)), max(1, int(h * scale))),
                          interpolation=cv2.INTER_AREA)
    orb = cv2.ORB_create(nfeatures=nfeatures)
    kps, descs = orb.detectAndCompute(gray, None)
    if descs is None or len(descs) < 8:
        return None, None
    kps_arr = np.array([[k.pt[0], k.pt[1]] for k in kps], dtype=np.float32)
    return descs.astype(np.uint8), kps_arr


# ---------------- 单文件处理 ----------------

# IMAGE_EXTS 内的优先级：用作 RAW companion 时按这个顺序挑分析源
_COMPANION_PRIORITY = [".jpg", ".jpeg", ".tif", ".tiff", ".png", ".heic", ".heif", ".webp", ".bmp"]


def _load_image_for_analysis(path: str, companions: list[str]) -> Image.Image:
    """加载用于分析的 PIL Image。

    - 普通图片（IMAGE_EXTS）：直接 Image.open。
    - RAW 文件（RAW_EXTS）：
      1. 优先用同 stem 同目录的 companion 图片（按 _COMPANION_PRIORITY 顺序）。
         这是 RAW+JPG 双拍工作流——直接用同名 JPG，零 RAW 解码开销。
      2. 没有 companion → 用 rawpy.extract_thumb() 提取 RAW 内嵌的 JPEG 预览图。
         所有主流 RAW 都内嵌全分辨率或半分辨率 JPEG，提取只需毫秒级（不做 demosaic）。

    返回已 .load() 的 PIL Image。任何失败都抛异常（由 _process_one 接住归 skipped）。
    """
    suffix = Path(path).suffix.lower()

    if suffix in IMAGE_EXTS:
        img = Image.open(path)
        img.load()
        return img

    if suffix not in RAW_EXTS:
        raise ValueError(f"不支持的文件类型：{suffix}")

    # primary 是 RAW —— 先找 companion
    best_comp: Optional[str] = None
    for ext in _COMPANION_PRIORITY:
        for c in companions:
            if Path(c).suffix.lower() == ext:
                best_comp = c
                break
        if best_comp:
            break

    if best_comp:
        try:
            img = Image.open(best_comp)
            img.load()
            return img
        except Exception as e:
            # JPG/JPEG companion 损坏（少见，但用户的 SD 卡读坏过）。
            # 优先级 1 失败了，自动 fallback 到 RAW 内嵌 JPEG——比直接 skip 整张图友好。
            import logging
            logging.getLogger("pic_selecter").warning(
                f"RAW {Path(path).name} 的 companion {Path(best_comp).name} 加载失败"
                f"（{type(e).__name__}: {e}），退回 RAW 内嵌 JPEG"
            )

    # 没 companion 或 companion 加载失败 → 提 RAW 内嵌 JPEG
    try:
        import rawpy
    except ImportError as e:
        raise RuntimeError(
            f"无法处理 RAW 文件 {Path(path).name}：未安装 rawpy。"
            f"请运行：pip install 'rawpy>=0.18'"
        ) from e

    with rawpy.imread(path) as raw:
        try:
            thumb = raw.extract_thumb()
        except (rawpy.LibRawNoThumbnailError, rawpy.LibRawUnsupportedThumbnailError) as e:
            raise RuntimeError(
                f"RAW 文件 {Path(path).name} 没有可用的内嵌预览图：{e}"
            ) from e

    if thumb.format == rawpy.ThumbFormat.JPEG:
        img = Image.open(io.BytesIO(thumb.data))
        img.load()
        return img
    if thumb.format == rawpy.ThumbFormat.BITMAP:
        return Image.fromarray(thumb.data)
    raise RuntimeError(f"RAW 内嵌缩略图格式不支持：{thumb.format}")


def _process_one(path: str, strength: str = "standard",
                 face_aware: bool = True,
                 engine: str = "expert",
                 llm_model: Optional[str] = None,
                 companions: Optional[list[str]] = None,
                 ) -> tuple[Optional[ImageInfo], Optional[str]]:
    """返回 (info, error_reason)。失败时 info=None。

    engine="expert"：DINOv2 + NIMA/MUSIQ/CLIP-IQA+ 美学 + InsightFace 人脸 + 本地拒片。
    engine="fast"：零模型路径，多 hash + HSV 直方图 + ORB + fast_quality。
    engine="tycoon"：DINOv2 + InsightFace（分组用）+ LLM 视觉判定（拒片用），
      llm_model 必须传入——从 list_models() 用户选定的 ID。

    path 是 primary 文件路径（RAW 或普通图片）。companions 是同 stem 同目录的
    其它文件（RAW 时优先用其中的 JPG 做分析；任何情况下搬运时一起搬）。
    """
    companions = companions or []
    try:
        st = os.stat(path)
    except OSError as e:
        return None, f"stat 失败: {e}"
    img = None
    try:
        try:
            img = _load_image_for_analysis(path, companions)
        except Exception as e:
            return None, f"加载失败: {type(e).__name__}: {e}"
        # EXIF 必须从原图读（含 width/height）
        ts_dt = _read_exif_datetime(img)
        exif_sum = extract_exif_summary(img, st.st_size)
        # 应用 EXIF 旋转，然后**一次性压到分析尺寸**，下游所有模型/算法共用：
        # 原图 5152×7728 直接喂 pyiqa/CLIP 会 MPS OOM；统一压一次最干净。
        img_t = _resize_for_analysis(ImageOps.exif_transpose(img))
        ph = imagehash.phash(img_t, hash_size=8)

        if engine == "fast":
            from pic_selecter.fast_quality import analyze_image_fast
            quality_info = analyze_image_fast(img_t, st.st_size, strength=strength)
            # 多 hash 签名 —— 四个 hash 都是必须的，任一失败让这张图归 skipped
            dh = str(imagehash.dhash(img_t, hash_size=8))
            wh = str(imagehash.whash(img_t, hash_size=8))
            ah = str(imagehash.average_hash(img_t, hash_size=8))
            # HSV 直方图 + ORB 描述子：cv2 在模块顶部已 import；图本身退化
            # （太小 / ORB 找不到关键点）会返回 None，由上层接住跳过该信号——
            # 这是"数据不足"，不是"能力降级"。
            color_hist = _compute_color_hist(img_t)
            orb_descs, orb_kps = _compute_orb(img_t)

            ts_unix = ts_dt.timestamp() if ts_dt else None
            return ImageInfo(
                path=path,
                companions=list(companions),
                phash=str(ph),
                timestamp=ts_unix,
                size=st.st_size,
                mtime=st.st_mtime,
                exif_summary=exif_sum,
                quality=quality_info.to_dict(),
                # 视觉模型字段在 fast 模式恒为 None（清晰区分模式）
                dhash=dh,
                whash=wh,
                ahash=ah,
                color_hist=color_hist,
                orb_descs=orb_descs,
                orb_kps=orb_kps,
            ), None

        if engine == "tycoon":
            # 土豪模式：DINOv2 + InsightFace 跑分组依赖；初筛交给 LLM
            if not llm_model:
                return None, "tycoon 缺少 llm_model 参数"
            import logging as _logging
            from pic_selecter import vision
            from pic_selecter import llm_judge
            from pic_selecter.quality import analyze_basic
            from pic_selecter.fast_quality import analyze_image_fast
            _log = _logging.getLogger("pic_selecter")

            dinov2_vec = vision.extract_dinov2(img_t)
            face_data = vision.extract_faces(img_t)
            face_embs = [f["embedding"] for f in face_data]

            # 进阶版极速预审：先用 fast_quality advanced 档拒明显废片，省下 LLM 调用。
            # 通过的才进 LLM；被拒的直接走 fast 的 auto_reject + reject_reason，
            # 并且不写 llm_verdict / llm_reason（UI 的 LLM 列会显示"缺失"，如实反映没调过 LLM）。
            fast_q = analyze_image_fast(img_t, st.st_size, strength="advanced")
            if fast_q.auto_reject:
                verdict = None
                reason = None
                quality_info = fast_q
                _log.info(
                    f"[tycoon] {Path(path).name}: fast-advanced 预审拒片 "
                    f"reason='{fast_q.reject_reason}' → 跳过 LLM"
                )
            else:
                # LLM 判定 —— 失败 3 次后抛 LLMJudgeError，让 _run_job 接住
                # strength 路由到 standard / advanced 两套 prompt
                judgement = llm_judge.judge_image(
                    img_t, model=llm_model, strength=strength,
                )
                verdict = judgement["verdict"]
                reason = judgement["reason"]

                quality_info = analyze_basic(
                    img_t, st.st_size,
                    llm_verdict=verdict,
                    llm_reason=reason,
                )

                _log.debug(
                    f"[tycoon] {Path(path).name}: "
                    f"verdict={verdict} reason='{reason}' "
                    f"faces={len(face_embs)}"
                )

            ts_unix = ts_dt.timestamp() if ts_dt else None
            return ImageInfo(
                path=path,
                companions=list(companions),
                phash=str(ph),
                timestamp=ts_unix,
                size=st.st_size,
                mtime=st.st_mtime,
                exif_summary=exif_sum,
                quality=quality_info.to_dict(),
                dinov2=dinov2_vec,
                face_embeddings=face_embs,
                llm_verdict=verdict,
                llm_reason=reason,
            ), None

        # ---- expert 分支 ----
        import logging as _logging
        _log = _logging.getLogger("pic_selecter")
        from pic_selecter import vision
        dinov2_vec = vision.extract_dinov2(img_t)
        aesthetic = vision.extract_aesthetic_score(img_t)
        musiq = vision.extract_musiq_score(img_t)
        clipiqa = vision.extract_clipiqa_score(img_t)
        face_data = vision.extract_faces(img_t)
        face_embs = [f["embedding"] for f in face_data]

        quality_info = analyze_image(
            img_t, st.st_size, strength=strength,
            face_aware=face_aware, face_data=face_data,
            aesthetic_score=aesthetic,
            musiq_score=musiq,
            clipiqa_score=clipiqa,
        )

        _log.debug(
            f"[expert] {Path(path).name}: "
            f"dinov2={'OK' if dinov2_vec is not None else 'FAIL'} "
            f"nima={aesthetic:.2f} musiq={musiq:.1f} clipiqa={clipiqa:.3f} "
            f"faces={len(face_embs)} "
            f"quality={quality_info.quality_score:.1f} "
            f"reject={quality_info.auto_reject}({quality_info.reject_reason})"
        )

        ts_unix = ts_dt.timestamp() if ts_dt else None
        return ImageInfo(
            path=path,
            companions=list(companions),
            phash=str(ph),
            timestamp=ts_unix,
            size=st.st_size,
            mtime=st.st_mtime,
            exif_summary=exif_sum,
            quality=quality_info.to_dict(),
            dinov2=dinov2_vec,
            aesthetic_score=aesthetic,
            musiq_score=musiq,
            clipiqa_score=clipiqa,
            face_embeddings=face_embs,
        ), None
    except Exception as e:
        return None, f"解码失败: {type(e).__name__}: {e}"
    finally:
        if img is not None:
            try:
                img.close()
            except Exception:
                pass


def scan_folder(folder: str) -> list[tuple[str, list[str]]]:
    """递归扫描所有受支持的文件，按 (目录, stem) 配对。

    返回 [(primary_path, companions), ...]：
    - 同 stem 同目录的所有文件视为一组
    - 优先把 RAW 当 primary（按 RAW_EXTS 字典序），其它文件作为 companions
    - 没有 RAW 的组：按 IMAGE_EXTS 字典序取第一个作 primary
    - 单个文件的组：companions 为空

    决策时只分析 primary 一个文件（RAW primary 会通过 _load_image_for_analysis
    用 companion JPG 或 RAW 内嵌预览来加载）。搬运时 primary 和所有 companions
    一起搬到相同目标目录、共享 stem。
    """
    p = Path(folder)
    # 按 (parent_dir, stem.lower()) 聚合候选文件
    groups: dict[tuple[str, str], list[str]] = {}
    for root, _, names in os.walk(p):
        rel = Path(root).relative_to(p)
        if rel.parts and rel.parts[0] in {"winners", "losers", "_pic_selecter"}:
            continue
        for n in names:
            suffix = Path(n).suffix.lower()
            if suffix not in ALL_INPUT_EXTS:
                continue
            full = str(Path(root) / n)
            key = (root, Path(n).stem.lower())
            groups.setdefault(key, []).append(full)

    result: list[tuple[str, list[str]]] = []
    for files in groups.values():
        files.sort()  # 稳定顺序
        raws = [f for f in files if Path(f).suffix.lower() in RAW_EXTS]
        non_raws = [f for f in files if Path(f).suffix.lower() in IMAGE_EXTS]
        if raws:
            primary = raws[0]
            companions = raws[1:] + non_raws
        else:
            # 不可能两个都空（groups 至少有一个元素）
            primary = non_raws[0]
            companions = non_raws[1:]
        result.append((primary, companions))
    result.sort(key=lambda t: t[0])
    return result


# ---------------- 计算入口 ----------------

def _default_expert_workers() -> int:
    """CUDA 环境下允许少量并发，让 CPU 前处理与 GPU 推理重叠。"""
    forced = os.getenv("PIC_SELECTER_EXPERT_WORKERS")
    if forced:
        try:
            return max(1, min(int(forced), 8))
        except ValueError:
            pass
    try:
        from pic_selecter import vision
        dev_type = vision._device_type()
    except Exception:
        dev_type = "cpu"
    # MPS / CPU 仍保持单线程，避免历史上的稳定性问题。
    if dev_type != "cuda":
        return 1
    return min(4, max(2, os.cpu_count() or 2))


def compute_infos(
    folder: str,
    workers: Optional[int] = None,
    progress: Optional[Callable[[int, int, str], None]] = None,
    cancel_check: Optional[Callable[[], bool]] = None,
    strength: str = "standard",
    face_aware: bool = True,
    event_cb: Optional[Callable[[str, str, Optional["ImageInfo"], Optional[str]], None]] = None,
    engine: str = "expert",
    llm_model: Optional[str] = None,
) -> tuple[list[ImageInfo], list[tuple[str, str]]]:
    """读取每张图片的 pHash + 时间戳 + EXIF 摘要，加上 engine 对应的额外签名。

    engine="fast"：算 dHash/wHash/aHash + HSV 直方图 + ORB 描述子 + fast_quality
    engine="expert"：算 DINOv2 + NIMA/MUSIQ/CLIP-IQA+ 美学 + InsightFace 人脸 + quality
    engine="tycoon"：算 DINOv2 + InsightFace（分组用）+ LLM 视觉判定（拒片用）。
      llm_model 必填，由前端通过 list_models() 取并由用户选定。

    返回 (info_list, skipped_list)，skipped_list 元素为 (path, reason)。
    """
    import logging
    log = logging.getLogger("pic_selecter")
    pairs = scan_folder(folder)
    companions_by_primary: dict[str, list[str]] = {p: c for p, c in pairs}
    files = [p for p, _ in pairs]
    raw_count = sum(1 for p in files if Path(p).suffix.lower() in RAW_EXTS)
    companion_count = sum(len(c) for c in companions_by_primary.values())
    log.info(
        f"[{engine}] scan_folder: 发现 {len(files)} 张 primary"
        f"（其中 RAW {raw_count} 张；伴随文件 {companion_count} 个）"
    )

    # ---- 启动期能力级检查：RAW 没 companion 必须能 import rawpy ----
    # 不静默降级：rawpy 缺失时让整任务挂，而不是把每张 RAW 默默 skip——
    # 后者会让用户对着"50 张照片只剩 10 张能处理"困惑半天。
    raw_without_jpg_companion = [
        p for p, comps in pairs
        if Path(p).suffix.lower() in RAW_EXTS
        and not any(Path(c).suffix.lower() in IMAGE_EXTS for c in comps)
    ]
    if raw_without_jpg_companion:
        try:
            import rawpy  # noqa: F401
        except ImportError as e:
            raise RuntimeError(
                f"发现 {len(raw_without_jpg_companion)} 个 RAW 文件没有同名 JPG，"
                f"必须安装 rawpy 才能处理：pip install 'rawpy>=0.18'。"
                f"（例：{Path(raw_without_jpg_companion[0]).name}）"
            ) from e
    needed: list[str] = []
    fresh: dict[str, ImageInfo] = {}
    skipped: list[tuple[str, str]] = []
    # 一次性运行：每次都重新分析，不读旧缓存
    for f in files:
        try:
            os.stat(f)
        except OSError as e:
            skipped.append((f, str(e)))
            continue
        needed.append(f)

    total = len(files)
    done = 0

    def _check_cancel():
        if cancel_check and cancel_check():
            raise CancelledError()

    if needed:
        # 工作线程数：
        # - expert：CUDA 下给少量并发把 GPU 喂起来；MPS / CPU 仍保守单线程
        # - tycoon：用 ARK_MAX_WORKERS（默认 20）作为 ThreadPool 上限；
        #          实际并发由 llm_judge._LIMITER 自适应控制（起 10、触发限流减半）
        # - fast：纯 CPU + numpy/cv2，开 8 线程没问题
        if workers is None:
            if engine == "expert":
                workers = _default_expert_workers()
            elif engine == "tycoon":
                workers = int(os.getenv("ARK_MAX_WORKERS", "20"))
                workers = max(1, min(workers, 32))
            else:
                workers = min(8, max(2, (os.cpu_count() or 4)))
        ex = ThreadPoolExecutor(max_workers=workers)
        try:
            futures = {
                ex.submit(
                    _process_one, f, strength, face_aware, engine, llm_model,
                    companions_by_primary.get(f, []),
                ): f
                for f in needed
            }
            # A6 修复：区分"能力级异常"（让整任务挂）和"图级异常"（这张 skip）
            # 能力级 = LLM 不可用 / vision 模型崩 / torch OOM / cv2 contrib 缺 ——
            # 这些通常意味着所有后续图都会同样失败，500 张图静默 skip 是最糟的体验。
            from concurrent.futures import CancelledError as _FutCancelled
            _capability_excs: tuple = ()
            try:
                from pic_selecter import llm_judge
                _capability_excs += (llm_judge.LLMJudgeError,)
            except Exception:
                pass
            try:
                from pic_selecter import vision as _vision_mod
                _capability_excs += (_vision_mod.VisionUnavailable,)
            except Exception:
                pass
            # torch OOM 名称随版本不同，按字符串识别更稳
            def _is_fatal_capability(exc: BaseException) -> bool:
                if _capability_excs and isinstance(exc, _capability_excs):
                    return True
                name = type(exc).__name__
                if name in {"OutOfMemoryError", "CUDAError"}:
                    return True
                msg = str(exc).lower()
                # ONNX/MPS 崩溃的特征字符串
                return any(k in msg for k in (
                    "out of memory", "cuda error", "mps backend out of memory",
                    "onnxruntime", "could not load library",
                ))

            for fut in as_completed(futures):
                _check_cancel()
                f = futures[fut]
                info = None
                reason: Optional[str] = None
                try:
                    result = fut.result()
                    if isinstance(result, tuple):
                        info, reason = result
                    else:
                        info = result
                except _FutCancelled:
                    raise CancelledError()
                except Exception as e:
                    if _is_fatal_capability(e):
                        log.error(
                            f"[{engine}] worker 遇到能力级异常，整任务终止："
                            f"{type(e).__name__}: {e}"
                        )
                        raise
                    reason = f"worker error: {type(e).__name__}: {e}"
                done += 1
                if info:
                    fresh[info.path] = info
                else:
                    skipped.append((f, reason or "未知原因"))
                if progress:
                    progress(done, total, Path(f).name)
                if event_cb:
                    try:
                        event_cb(Path(f).name, f, info, reason)
                    except Exception:
                        pass
        finally:
            ex.shutdown(wait=False, cancel_futures=True)

    result_list = [fresh[f] for f in files if f in fresh]

    # ---- 总结日志：让用户在 log.txt 里直接看见各信号实际成功率 ----
    if result_list:
        if engine == "fast":
            n_color = sum(1 for i in result_list if i.color_hist is not None)
            n_orb = sum(1 for i in result_list if i.orb_descs is not None)
            n_whash = sum(1 for i in result_list if i.whash is not None)
            log.info(
                f"[fast] compute_infos 完成：成功 {len(result_list)} / 跳过 {len(skipped)}；"
                f"HSV {n_color} ({n_color * 100 // max(1, len(result_list))}%) · "
                f"ORB {n_orb} ({n_orb * 100 // max(1, len(result_list))}%) · "
                f"wHash {n_whash} ({n_whash * 100 // max(1, len(result_list))}%)"
            )
        else:
            n_dino = sum(1 for i in result_list if i.dinov2 is not None)
            n_aes = sum(1 for i in result_list if i.aesthetic_score is not None)
            n_face = sum(1 for i in result_list if i.face_embeddings)
            total = max(1, len(result_list))
            log.info(
                f"[expert] compute_infos 完成：成功 {len(result_list)} / 跳过 {len(skipped)}；"
                f"DINOv2 {n_dino} ({n_dino * 100 // total}%) · "
                f"NIMA {n_aes} ({n_aes * 100 // total}%) · "
                f"有脸 {n_face} ({n_face * 100 // total}%)"
            )
    return result_list, skipped


# ---------------- 分组分发 ----------------

def group_infos(
    infos: list[ImageInfo],
    threshold_near: int = THRESHOLD_NEAR,
    threshold_far: int = THRESHOLD_FAR,
    near_seconds: int = NEAR_SECONDS,
    engine: str = "expert",
) -> list[list[ImageInfo]]:
    """分组分发器。失败直接抛——不再静默回退。

    engine="fast"：fast_clustering.cluster（4 hash + HSV + ORB 几何验证 + 时间硬切段）
    engine="expert" / "tycoon"：clustering.cluster（DINOv2 + 多信号融合）。
      tycoon 在分组维度上和 expert 等价（都靠 DINOv2 + InsightFace），
      区别只在初筛走 LLM 而非本地拒片。
    """
    import logging
    log = logging.getLogger("pic_selecter")
    if not infos:
        return []
    if len(infos) == 1:
        log.info(f"[{engine}] group_infos: 单图直接成组")
        return [[infos[0]]]
    log.info(f"[{engine}] group_infos: 开始聚类 {len(infos)} 张")
    if engine == "fast":
        from pic_selecter import fast_clustering
        idx_groups = fast_clustering.cluster(infos)
    elif engine in ("expert", "tycoon"):
        from pic_selecter import clustering
        idx_groups = clustering.cluster(infos)
    else:
        raise ValueError(f"未知 engine: {engine!r}（仅支持 'fast' / 'expert' / 'tycoon'）")
    sizes = sorted((len(g) for g in idx_groups), reverse=True)
    multi = sum(1 for g in idx_groups if len(g) > 1)
    log.info(
        f"[{engine}] group_infos: 输出 {len(idx_groups)} 组（多图组 {multi}，"
        f"前 5 大={sizes[:5]}）"
    )
    return [[infos[i] for i in g] for g in idx_groups]


def progress_printer(done: int, total: int, label: str) -> None:
    bar_len = 30
    frac = done / total if total else 1
    filled = int(bar_len * frac)
    bar = "#" * filled + "-" * (bar_len - filled)
    print(f"\r[{bar}] {done}/{total}  {label[:40]:<40}", end="", flush=True)
    if done == total:
        print()


def build_groups(
    folder: str,
    progress: Callable = progress_printer,
    cancel_check: Optional[Callable[[], bool]] = None,
    threshold_near: int = THRESHOLD_NEAR,
    threshold_far: int = THRESHOLD_FAR,
    near_seconds: int = NEAR_SECONDS,
) -> tuple[list[list[ImageInfo]], list[tuple[str, str]]]:
    infos, skipped = compute_infos(folder, progress=progress, cancel_check=cancel_check)
    groups = group_infos(
        infos,
        threshold_near=threshold_near,
        threshold_far=threshold_far,
        near_seconds=near_seconds,
    )
    return groups, skipped


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("用法: python grouper.py <folder>")
        sys.exit(1)
    target = sys.argv[1]
    print(f"扫描中: {target}")
    groups, skipped = build_groups(target)
    if skipped:
        print(f"\n跳过 {len(skipped)} 个文件：")
        for p, r in skipped[:10]:
            print(f"  {Path(p).name}: {r}")
        if len(skipped) > 10:
            print(f"  ... 还有 {len(skipped) - 10} 个")
    print(f"\n共 {sum(len(g) for g in groups)} 张照片，分为 {len(groups)} 组")
    multi = [g for g in groups if len(g) > 1]
    print(f"其中有相似关系的组: {len(multi)} 个，单图组: {len(groups) - len(multi)} 个")
    for i, g in enumerate(multi[:10], 1):
        print(f"  组 {i}: {len(g)} 张")
        for info in g[:5]:
            t = datetime.fromtimestamp(info.timestamp).isoformat() if info.timestamp else "无时间"
            print(f"    {Path(info.path).name}  {info.phash}  {t}")
        if len(g) > 5:
            print(f"    ... 还有 {len(g) - 5} 张")
