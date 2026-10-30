// iDFilm — 후지필름 감성 이미지 보정 백엔드.
// 원본은 메모리에 들고(최대 해상도), 미리보기는 긴 변 1400px로 줄여서
// 슬라이더가 움직일 때마다 미리보기 버퍼에만 파이프라인을 돌린다 (빠름).
// 저장할 때만 원본 해상도에 같은 파이프라인을 적용한다.
// 3D LUT(HaldCLUT PNG / .cube)를 로드하면 "lut" 필름으로 그대로 적용된다.

mod process;

use std::io::Cursor;
use std::sync::Mutex;

use base64::Engine as _;
use image::imageops::FilterType;
use image::{ImageFormat, RgbImage};
use serde::Serialize;
use tauri::State;

use process::{Lut3d, Params};

/// 미리보기 긴 변 (px)
const PREVIEW_MAX: u32 = 1400;

/// 내장 필름 LUT — 빌드에 임베드되어 항상 사용 가능 (형이 추출한 Fuji Style)
fn builtin_fuji() -> Option<&'static Lut3d> {
    static FUJI: std::sync::OnceLock<Option<Lut3d>> = std::sync::OnceLock::new();
    FUJI.get_or_init(|| {
        process::parse_cube(include_str!("../luts/Fuji_Style.cube"))
            .ok()
            // 옛 추출 방식으로 구운 큐브라 셀 노이즈가 있다 — 다항식 리핏으로
            // 매끈한 LUT로 재생성 (밴딩·중립색 오염 제거)
            .map(|l| process::refit_lut(&l))
    })
    .as_ref()
}

/// 필름 id에 맞는 LUT 선택 — 내장 필름이면 임베드 LUT, "lut"이면 사용자 로드 LUT
fn select_lut<'a>(film: &str, user: &'a Option<Lut3d>) -> Option<&'a Lut3d> {
    match film {
        "fuji_style" => builtin_fuji(),
        "lut" => user.as_ref(),
        _ => None,
    }
}

struct Loaded {
    /// 원본 RGB (EXIF 회전 반영됨)
    full: RgbImage,
    /// 미리보기 크기로 줄인 원본
    preview: RgbImage,
}

#[derive(Default)]
struct ImgState {
    img: Mutex<Option<Loaded>>,
    lut: Mutex<Option<Lut3d>>,
    /// 폴더 일괄 처리 취소 플래그
    batch_cancel: std::sync::atomic::AtomicBool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenResult {
    width: u32,
    height: u32,
    /// 보정 전 원본 미리보기 (data URL)
    preview: String,
}

fn err(e: impl std::fmt::Display) -> String {
    format!("{e}")
}

/// JPEG EXIF Orientation을 읽어 회전을 돌려준다 (3=180, 6=90CW, 8=270CW).
fn exif_orientation(path: &str) -> u32 {
    let Ok(file) = std::fs::File::open(path) else { return 1 };
    let mut reader = std::io::BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) else { return 1 };
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(1)
}

fn apply_orientation(img: RgbImage, o: u32) -> RgbImage {
    match o {
        3 => image::imageops::rotate180(&img),
        6 => image::imageops::rotate90(&img),
        8 => image::imageops::rotate270(&img),
        _ => img,
    }
}

/// RGB 버퍼 → JPEG data URL
fn to_data_url(img: &RgbImage, quality: u8) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut cursor = Cursor::new(&mut bytes);
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
    img.write_with_encoder(enc).map_err(err)?;
    Ok(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

fn make_preview(full: &RgbImage) -> RgbImage {
    let (w, h) = full.dimensions();
    let long = w.max(h);
    if long <= PREVIEW_MAX {
        return full.clone();
    }
    let scale = PREVIEW_MAX as f32 / long as f32;
    let (nw, nh) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
    image::imageops::resize(full, nw.max(1), nh.max(1), FilterType::Triangle)
}

#[tauri::command(async)]
fn open_image(state: State<ImgState>, path: String) -> Result<OpenResult, String> {
    let img = image::open(&path).map_err(|e| format!("이미지를 열 수 없습니다: {e}"))?;
    let rgb = img.to_rgb8();
    let rgb = apply_orientation(rgb, exif_orientation(&path));
    let preview = make_preview(&rgb);
    let (width, height) = rgb.dimensions();
    let preview_url = to_data_url(&preview, 90)?;
    *state.img.lock().unwrap() = Some(Loaded { full: rgb, preview });
    Ok(OpenResult { width, height, preview: preview_url })
}

/// 3D LUT 로드 (.cube 또는 HaldCLUT PNG). 성공하면 LUT 정보 문자열 반환.
#[tauri::command(async)]
fn load_lut(state: State<ImgState>, path: String) -> Result<String, String> {
    let lower = path.to_lowercase();
    let mut lut = if lower.ends_with(".cube") {
        let text = std::fs::read_to_string(&path).map_err(err)?;
        process::parse_cube(&text)?
    } else {
        let img = image::open(&path).map_err(|e| format!("LUT 이미지를 열 수 없습니다: {e}"))?;
        let rgb = img.to_rgb8();
        let (w, h) = rgb.dimensions();
        process::parse_hald(rgb.as_raw(), w, h)?
    };
    // 가벼운 스무딩 — 셀 노이즈로 인한 밴딩 방지 (정밀 제작 LUT에는 체감 영향 없음)
    process::smooth_lut(&mut lut, 1, 0.75);
    let info = format!("{}³", lut.size);
    *state.lut.lock().unwrap() = Some(lut);
    Ok(info)
}

/// 공통 비율로 중앙 크롭 (전/후 캡처의 크롭 차이 보정 1단계)
fn crop_to_aspect(img: &RgbImage, aspect: f32) -> RgbImage {
    let (w, h) = img.dimensions();
    let cur = w as f32 / h as f32;
    let (nw, nh) = if cur > aspect {
        (((h as f32) * aspect) as u32, h)
    } else {
        (w, ((w as f32) / aspect) as u32)
    };
    let (nw, nh) = (nw.max(1), nh.max(1));
    image::imageops::crop_imm(img, (w - nw) / 2, (h - nh) / 2, nw, nh).to_image()
}

/// 밝기 기반 평행이동 탐색 (전/후 캡처의 크롭 차이 보정 2단계).
/// 두 그레이 버퍼에서 after를 (dx,dy)만큼 밀었을 때 평균 밝기 차가 최소인 오프셋.
fn align_offset(bg: &[f32], ag: &[f32], w: usize, h: usize, range: i32) -> (i32, i32) {
    let mut best = (0i32, 0i32);
    let mut best_score = f32::MAX;
    let margin = range as usize + 1;
    let step_eval = 2usize; // 평가 픽셀 샘플링 (속도)
    for dy in -range..=range {
        for dx in -range..=range {
            let mut sum = 0f32;
            let mut cnt = 0u32;
            let mut y = margin;
            while y < h - margin {
                let mut x = margin;
                while x < w - margin {
                    let bx = bg[y * w + x];
                    let ax = ag[(y as i32 + dy) as usize * w + (x as i32 + dx) as usize];
                    sum += (bx - ax).abs();
                    cnt += 1;
                    x += step_eval;
                }
                y += step_eval;
            }
            let score = sum / cnt.max(1) as f32;
            if score < best_score {
                best_score = score;
                best = (dx, dy);
            }
        }
    }
    best
}

/// 보정 전/후 사진 쌍에서 색 변환(LUT)을 추출한다.
/// 크롭이 조금 달라도 되도록 ① 공통 비율 중앙 크롭 ② 밝기 기반 자동 정렬을
/// 거친 뒤, 겹치는 영역의 픽셀 쌍만으로 LUT를 피팅한다.
#[tauri::command(async)]
fn extract_lut(state: State<ImgState>, before: String, after: String) -> Result<String, String> {
    let load = |p: &str| -> Result<RgbImage, String> {
        let img = image::open(p).map_err(|e| format!("이미지를 열 수 없습니다: {e}"))?;
        Ok(apply_orientation(img.to_rgb8(), exif_orientation(p)))
    };
    let b = load(&before)?;
    let a = load(&after)?;

    // ① 두 사진을 공통(더 좁은) 비율로 중앙 크롭 → 같은 크기로 리사이즈
    let (bw, bh) = b.dimensions();
    let (aw, ah) = a.dimensions();
    let aspect = (bw as f32 / bh as f32).min(aw as f32 / ah as f32);
    let b = crop_to_aspect(&b, aspect);
    let a = crop_to_aspect(&a, aspect);
    let (bw, bh) = b.dimensions();
    // 640px: 격자당 샘플 수를 늘려 평균 노이즈(물·하늘 얼룩)를 줄인다
    let scale = (640.0 / bw.max(bh) as f32).min(1.0);
    let (w, h) = (
        ((bw as f32 * scale) as u32).max(32),
        ((bh as f32 * scale) as u32).max(32),
    );
    let bs = image::imageops::resize(&b, w, h, FilterType::Triangle);
    let as_ = image::imageops::resize(&a, w, h, FilterType::Triangle);

    // ② 밝기 기반 평행이동 정렬 (±4% 범위)
    let gray = |img: &RgbImage| -> Vec<f32> {
        img.as_raw()
            .chunks_exact(3)
            .map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
            .collect()
    };
    let (w_us, h_us) = (w as usize, h as usize);
    let range = ((w_us.min(h_us) as f32 * 0.04) as i32).clamp(2, 16);
    let (dx, dy) = align_offset(&gray(&bs), &gray(&as_), w_us, h_us, range);

    // ③ 겹치는 영역의 픽셀 쌍 수집
    let braw = bs.as_raw();
    let araw = as_.as_raw();
    let mut src = Vec::with_capacity(braw.len());
    let mut dst = Vec::with_capacity(braw.len());
    let m = range as usize + 1;
    for y in m..h_us - m {
        for x in m..w_us - m {
            let bi = (y * w_us + x) * 3;
            let ai = ((y as i32 + dy) as usize * w_us + (x as i32 + dx) as usize) * 3;
            src.extend_from_slice(&braw[bi..bi + 3]);
            dst.extend_from_slice(&araw[ai..ai + 3]);
        }
    }
    let count = src.len() / 3;
    if count < 1000 {
        return Err("겹치는 영역이 너무 작습니다 — 같은 사진의 전/후를 선택하세요".into());
    }

    // 3차 다항식 피팅 — 매끈함이 구조적으로 보장돼 밴딩·중립색 오염이 없다
    let lut = process::fit_smooth(&src, &dst, count)?;
    let info = format!("{}³ (전/후 추출, 정렬 {dx},{dy})", lut.size);
    *state.lut.lock().unwrap() = Some(lut);
    Ok(info)
}

/// 현재 로드/추출된 LUT를 .cube 파일로 내보낸다.
#[tauri::command(async)]
fn export_lut(state: State<ImgState>, path: String) -> Result<(), String> {
    let guard = state.lut.lock().unwrap();
    let lut = guard.as_ref().ok_or("내보낼 LUT가 없습니다")?;
    std::fs::write(&path, process::to_cube(lut, "iDFilm extracted")).map_err(err)
}

/// 현재 설정으로 미리보기 렌더 (data URL 반환)
#[tauri::command(async)]
fn render_preview(state: State<ImgState>, params: Params) -> Result<String, String> {
    let guard = state.img.lock().unwrap();
    let loaded = guard.as_ref().ok_or("열린 이미지가 없습니다")?;
    let lut_guard = state.lut.lock().unwrap();
    let (w, h) = loaded.preview.dimensions();
    let out = process::apply(loaded.preview.as_raw(), w, h, &params, select_lut(&params.film, &lut_guard));
    let img = RgbImage::from_raw(w, h, out).ok_or("버퍼 오류")?;
    to_data_url(&img, 88)
}

/// 원본 해상도로 보정 렌더 (1:1 보기용 data URL)
#[tauri::command(async)]
fn render_full(state: State<ImgState>, params: Params) -> Result<String, String> {
    let guard = state.img.lock().unwrap();
    let loaded = guard.as_ref().ok_or("열린 이미지가 없습니다")?;
    let lut_guard = state.lut.lock().unwrap();
    let (w, h) = loaded.full.dimensions();
    let out = process::apply(loaded.full.as_raw(), w, h, &params, select_lut(&params.film, &lut_guard));
    let img = RgbImage::from_raw(w, h, out).ok_or("버퍼 오류")?;
    to_data_url(&img, 90)
}

/// 원본(보정 전) 전체 해상도 data URL (1:1 비교용)
#[tauri::command(async)]
fn original_full(state: State<ImgState>) -> Result<String, String> {
    let guard = state.img.lock().unwrap();
    let loaded = guard.as_ref().ok_or("열린 이미지가 없습니다")?;
    to_data_url(&loaded.full, 92)
}

/// 원본 해상도에 적용해서 저장. 확장자(jpg/png)에 따라 포맷 결정.
#[tauri::command(async)]
fn save_image(state: State<ImgState>, path: String, params: Params) -> Result<(), String> {
    let guard = state.img.lock().unwrap();
    let loaded = guard.as_ref().ok_or("열린 이미지가 없습니다")?;
    let lut_guard = state.lut.lock().unwrap();
    let (w, h) = loaded.full.dimensions();
    let out = process::apply(loaded.full.as_raw(), w, h, &params, select_lut(&params.film, &lut_guard));
    let img = RgbImage::from_raw(w, h, out).ok_or("버퍼 오류")?;

    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        img.save_with_format(&path, ImageFormat::Png).map_err(err)
    } else {
        let file = std::fs::File::create(&path).map_err(err)?;
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 92);
        img.write_with_encoder(enc).map_err(err)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImgEntry {
    name: String,
    path: String,
}

/// 폴더 안의 이미지 파일 목록 (이름순)
#[tauri::command(async)]
fn list_images(dir: String) -> Result<Vec<ImgEntry>, String> {
    const EXTS: [&str; 5] = ["jpg", "jpeg", "png", "bmp", "webp"];
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("폴더를 열 수 없습니다: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|x| x.to_str())
                    .map(|x| EXTS.contains(&x.to_lowercase().as_str()))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    Ok(files
        .into_iter()
        .filter_map(|p| {
            Some(ImgEntry {
                name: p.file_name()?.to_str()?.to_string(),
                path: p.to_str()?.to_string(),
            })
        })
        .collect())
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BatchProgress {
    done: usize,
    total: usize,
    name: String,
}

/// 선택된 이미지들에 현재 설정을 적용해 `dir/out_name/`에 저장한다.
/// 진행 상황은 "batch-progress" 이벤트로 프런트에 알린다.
#[tauri::command(async)]
fn batch_process(
    app: tauri::AppHandle,
    state: State<ImgState>,
    dir: String,
    files: Vec<String>,
    params: Params,
    out_name: String,
) -> Result<String, String> {
    use std::sync::atomic::Ordering;
    use tauri::Emitter;

    state.batch_cancel.store(false, Ordering::Relaxed);

    // LUT 스냅샷 (긴 작업 동안 뮤텍스를 잡고 있지 않도록 복제)
    let user_lut: Option<Lut3d> = state.lut.lock().unwrap().clone();
    let lut_ref: Option<&Lut3d> = match params.film.as_str() {
        "fuji_style" => builtin_fuji(),
        "lut" => user_lut.as_ref(),
        _ => None,
    };

    let files: Vec<std::path::PathBuf> = files.into_iter().map(std::path::PathBuf::from).collect();
    let total = files.len();
    if total == 0 {
        return Err("선택된 이미지가 없습니다".into());
    }

    let out_dir = std::path::Path::new(&dir).join(out_name.trim());
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("출력 폴더 생성 실패: {e}"))?;

    let mut ok = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for (i, path) in files.iter().enumerate() {
        if state.batch_cancel.load(Ordering::Relaxed) {
            return Ok(format!("취소됨 — {ok}/{total}장 완료 (저장 위치: {})", out_dir.display()));
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let _ = app.emit("batch-progress", BatchProgress { done: i, total, name: name.clone() });

        let result = (|| -> Result<(), String> {
            let p = path.to_str().ok_or("경로 오류")?;
            let img = image::open(p).map_err(err)?;
            let rgb = apply_orientation(img.to_rgb8(), exif_orientation(p));
            let (w, h) = rgb.dimensions();
            let out = process::apply(rgb.as_raw(), w, h, &params, lut_ref);
            let img = RgbImage::from_raw(w, h, out).ok_or("버퍼 오류")?;

            let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("jpg").to_lowercase();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
            if ext == "png" {
                let out_path = out_dir.join(format!("{stem}.png"));
                img.save_with_format(&out_path, ImageFormat::Png).map_err(err)
            } else {
                // jpg/jpeg 유지, bmp/webp는 jpg로 저장
                let out_path = out_dir.join(format!("{stem}.jpg"));
                let file = std::fs::File::create(&out_path).map_err(err)?;
                let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 92);
                img.write_with_encoder(enc).map_err(err)
            }
        })();
        match result {
            Ok(()) => ok += 1,
            Err(_) => failed.push(name),
        }
    }
    let _ = app.emit("batch-progress", BatchProgress { done: total, total, name: String::new() });

    let mut msg = format!("{ok}/{total}장 완료 (저장 위치: {})", out_dir.display());
    if !failed.is_empty() {
        msg += &format!(" · 실패 {}장: {}", failed.len(), failed.join(", "));
    }
    Ok(msg)
}

/// 진행 중인 일괄 처리 취소 요청
#[tauri::command]
fn batch_cancel(state: State<ImgState>) {
    state.batch_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(ImgState::default())
        .invoke_handler(tauri::generate_handler![
            open_image,
            load_lut,
            extract_lut,
            export_lut,
            render_preview,
            render_full,
            original_full,
            save_image,
            list_images,
            batch_process,
            batch_cancel
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
