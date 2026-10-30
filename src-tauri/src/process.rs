// 이미지 보정 파이프라인 v2 — 필름 그레이딩 엔진.
//
// v1(단순 S커브+채도)이 "인스타 필터" 느낌이었던 걸 고쳐, 실제 필름 시뮬레이션이
// 하는 일을 흉내낸다:
//   · 필름 톤커브: 토(toe)/숄더가 비대칭인 제어점 스플라인 (모노톤 큐빅 보간)
//   · 색상(hue)별 선택 조정: 특정 색만 채도/밝기/색상 이동 (예: Classic Chrome의
//     "파랑 죽이기", 네거 필름의 "초록→올리브")
//   · 스플릿 토닝: 그림자/하이라이트에 서로 다른 틴트
//   · 컬러 크롬: 밝고 채도 높은 영역의 밝기를 눌러 깊이감
//   · 2옥타브 그레인: 픽셀 노이즈가 아니라 뭉친 필름 입자 느낌
//
// 또한 3D LUT(HaldCLUT PNG / .cube)를 로드해 그대로 적용할 수 있다 — 실측 필름
// LUT를 쓰면 근사가 아닌 진짜 필름 색이 나온다.

use serde::Deserialize;

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub film: String,
    pub exposure: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub temperature: f32,
    pub sharpen: f32,
    pub grain: f32,
    pub vignette: f32,
    /// 피부 보정 (0..1) — 피부색 영역의 잡티·톤 얼룩 스무딩 (큰 윤곽은 유지)
    #[serde(default)]
    pub skin: f32,
}

#[inline]
fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

#[inline]
fn luma(r: f32, g: f32, b: f32) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

// ---- RGB <-> HSL ---------------------------------------------------------------

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < 1e-6 {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if (max - g).abs() < 1e-6 {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    (h * 360.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let h = ((h % 360.0) + 360.0) % 360.0 / 360.0;
    if s < 1e-6 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

// ---- 모노톤 큐빅 스플라인 (톤커브) ----------------------------------------------
// 제어점을 지나되 출렁임(오버슈트)이 없는 Fritsch-Carlson 방식.

fn curve_lut(points: &[(f32, f32)]) -> [f32; 256] {
    let n = points.len();
    let mut lut = [0f32; 256];
    if n < 2 {
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as f32 / 255.0;
        }
        return lut;
    }
    // 구간 기울기
    let mut d = vec![0f32; n - 1];
    for i in 0..n - 1 {
        let dx = points[i + 1].0 - points[i].0;
        d[i] = if dx.abs() < 1e-6 { 0.0 } else { (points[i + 1].1 - points[i].1) / dx };
    }
    // 접선 (모노톤 보정)
    let mut m = vec![0f32; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for i in 1..n - 1 {
        m[i] = if d[i - 1] * d[i] <= 0.0 { 0.0 } else { (d[i - 1] + d[i]) / 2.0 };
    }
    for i in 0..n - 1 {
        if d[i].abs() < 1e-6 {
            m[i] = 0.0;
            m[i + 1] = 0.0;
        } else {
            let a = m[i] / d[i];
            let b = m[i + 1] / d[i];
            let s = (a * a + b * b).sqrt();
            if s > 3.0 {
                m[i] = 3.0 * d[i] * a / s;
                m[i + 1] = 3.0 * d[i] * b / s;
            }
        }
    }
    for (idx, v) in lut.iter_mut().enumerate() {
        let x = idx as f32 / 255.0;
        // 구간 찾기
        let mut seg = n - 2;
        for i in 0..n - 1 {
            if x <= points[i + 1].0 {
                seg = i;
                break;
            }
        }
        let (x0, y0) = points[seg];
        let (x1, y1) = points[seg + 1];
        let hh = (x1 - x0).max(1e-6);
        let t = ((x - x0) / hh).clamp(0.0, 1.0);
        let t2 = t * t;
        let t3 = t2 * t;
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        *v = clamp01(h00 * y0 + h10 * hh * m[seg] + h01 * y1 + h11 * hh * m[seg + 1]);
    }
    lut
}

// ---- 필름 레시피 ----------------------------------------------------------------

/// 색상별 조정: center(도) 중심 ±sigma 범위의 색만 채도 배율·색상 이동·밝기 가감
struct HueAdj {
    center: f32,
    sigma: f32,
    sat: f32,  // 채도 배율
    hue: f32,  // 색상 이동 (도)
    lum: f32,  // 밝기 가감
}

struct Recipe {
    curve: &'static [(f32, f32)],
    sat: f32,
    /// 필름 자체 화이트밸런스 (R, B 게인)
    wb: (f32, f32),
    hues: &'static [HueAdj],
    shadow_tint: (f32, f32, f32),
    highlight_tint: (f32, f32, f32),
    /// 컬러 크롬 효과 강도 (밝고 채도 높은 영역 밝기 누르기)
    chrome: f32,
    /// 흑백 (acros)
    bw: bool,
}

const NO_HUES: &[HueAdj] = &[];

fn recipe(name: &str) -> Recipe {
    match name {
        // Velvia — 벨벳 같은 고채도·깊은 그림자. 초록·파랑이 특히 진해진다.
        "velvia" => Recipe {
            curve: &[(0.0, 0.0), (0.18, 0.11), (0.5, 0.52), (0.82, 0.90), (1.0, 1.0)],
            sat: 1.30,
            wb: (1.0, 1.0),
            hues: &[
                HueAdj { center: 120.0, sigma: 45.0, sat: 1.18, hue: -4.0, lum: -0.02 },
                HueAdj { center: 225.0, sigma: 40.0, sat: 1.22, hue: -6.0, lum: -0.03 },
                HueAdj { center: 0.0, sigma: 30.0, sat: 1.12, hue: -3.0, lum: 0.0 },
            ],
            shadow_tint: (0.0, 0.0, 0.004),
            highlight_tint: (0.0, 0.0, 0.0),
            chrome: 0.5,
            bw: false,
        },
        // Astia — 부드러운 하이라이트, 피부톤 친화. 채도는 있는데 콘트라스트는 순함.
        "astia" => Recipe {
            curve: &[(0.0, 0.015), (0.25, 0.25), (0.5, 0.53), (0.78, 0.80), (1.0, 0.965)],
            sat: 1.10,
            wb: (1.012, 0.995),
            hues: &[
                HueAdj { center: 30.0, sigma: 25.0, sat: 0.94, hue: 2.0, lum: 0.015 }, // 피부
                HueAdj { center: 215.0, sigma: 35.0, sat: 1.10, hue: 0.0, lum: 0.0 },  // 하늘
            ],
            shadow_tint: (0.0, 0.0, 0.0),
            highlight_tint: (0.004, 0.002, 0.0),
            chrome: 0.3,
            bw: false,
        },
        // Classic Chrome — 다큐 감성의 핵심: 파랑을 죽이고 어둡게, 빨강은 주황 쪽으로,
        // 그림자는 청록빛, 하이라이트는 일찍 눌린다.
        "classic_chrome" => Recipe {
            curve: &[(0.0, 0.0), (0.2, 0.17), (0.45, 0.47), (0.72, 0.76), (1.0, 0.93)],
            sat: 0.82,
            wb: (0.995, 0.99),
            hues: &[
                HueAdj { center: 225.0, sigma: 45.0, sat: 0.62, hue: -8.0, lum: -0.05 }, // 파랑 죽이기
                HueAdj { center: 0.0, sigma: 28.0, sat: 0.90, hue: 10.0, lum: 0.0 },     // 빨강→주황
                HueAdj { center: 120.0, sigma: 45.0, sat: 0.80, hue: 8.0, lum: -0.01 },  // 초록 올리브로
                HueAdj { center: 55.0, sigma: 22.0, sat: 0.85, hue: -6.0, lum: 0.0 },    // 노랑 눌림
            ],
            shadow_tint: (-0.010, 0.004, 0.014),
            highlight_tint: (0.003, 0.001, -0.004),
            chrome: 1.0,
            bw: false,
        },
        // Classic Neg — 네거 필름의 거친 콘트라스트: 들린 블랙, 초록→올리브,
        // 그림자 시안 / 하이라이트 웜 스플릿.
        "classic_neg" => Recipe {
            curve: &[(0.0, 0.028), (0.22, 0.17), (0.5, 0.51), (0.78, 0.85), (1.0, 0.97)],
            sat: 0.86,
            wb: (1.005, 0.985),
            hues: &[
                HueAdj { center: 120.0, sigma: 45.0, sat: 0.72, hue: 14.0, lum: -0.02 },
                HueAdj { center: 225.0, sigma: 40.0, sat: 0.80, hue: -5.0, lum: -0.02 },
                HueAdj { center: 30.0, sigma: 24.0, sat: 0.92, hue: -4.0, lum: 0.01 },
            ],
            shadow_tint: (-0.012, 0.005, 0.012),
            highlight_tint: (0.010, 0.002, -0.006),
            chrome: 0.7,
            bw: false,
        },
        // Eterna — 시네마: 아주 낮은 채도·대비, 들린 그림자, 어디에도 튀는 색 없음.
        "eterna" => Recipe {
            curve: &[(0.0, 0.04), (0.25, 0.26), (0.5, 0.50), (0.75, 0.72), (1.0, 0.915)],
            sat: 0.72,
            wb: (1.0, 1.0),
            hues: &[HueAdj { center: 0.0, sigma: 30.0, sat: 0.92, hue: 2.0, lum: 0.0 }],
            shadow_tint: (-0.004, 0.002, 0.006),
            highlight_tint: (0.0, 0.0, 0.0),
            chrome: 0.9,
            bw: false,
        },
        // Vintage Warm — 따뜻하고 색이 풍부한 필름 (포트라 계열).
        // 바래지 않는다: 주황·빨강·파랑(청바지)은 오히려 채도를 올리고,
        // 물(청록)만 살짝 따뜻한 녹색으로, 전체 웜 캐스트는 은은하게.
        "vintage_warm" => Recipe {
            curve: &[(0.0, 0.02), (0.22, 0.20), (0.5, 0.53), (0.8, 0.85), (1.0, 0.97)],
            sat: 1.10,
            wb: (1.022, 0.972),
            hues: &[
                HueAdj { center: 38.0, sigma: 24.0, sat: 1.55, hue: -5.0, lum: 0.015 },  // 노랑→진한 골드
                HueAdj { center: 12.0, sigma: 18.0, sat: 1.45, hue: 0.0, lum: -0.01 },   // 빨강 더 깊고 진하게
                HueAdj { center: 225.0, sigma: 35.0, sat: 1.10, hue: -6.0, lum: 0.0 },   // 파랑(청바지) 유지·살짝 시안
                HueAdj { center: 185.0, sigma: 35.0, sat: 0.82, hue: -12.0, lum: -0.01 }, // 물만 살짝 따뜻한 녹색
                HueAdj { center: 110.0, sigma: 40.0, sat: 1.08, hue: 12.0, lum: -0.02 }, // 초록 → 가을 올리브
            ],
            shadow_tint: (0.006, 0.004, -0.002),
            highlight_tint: (0.010, 0.007, -0.004),
            chrome: 0.25,
            bw: false,
        },
        // Acros — 흑백. 붉은 감도 가중 + 깊은 블랙, 부드러운 숄더.
        "acros" => Recipe {
            curve: &[(0.0, 0.0), (0.18, 0.12), (0.5, 0.50), (0.82, 0.89), (1.0, 0.985)],
            sat: 1.0,
            wb: (1.0, 1.0),
            hues: NO_HUES,
            shadow_tint: (0.0, 0.0, 0.0),
            highlight_tint: (0.0, 0.0, 0.0),
            chrome: 0.0,
            bw: true,
        },
        // Provia(표준) 및 그 외 — 중립이지만 필름다운 토/숄더와 은은한 채도.
        _ => Recipe {
            curve: &[(0.0, 0.004), (0.22, 0.19), (0.5, 0.52), (0.8, 0.84), (1.0, 0.98)],
            sat: 1.08,
            wb: (1.0, 1.0),
            hues: &[HueAdj { center: 225.0, sigma: 40.0, sat: 1.05, hue: 0.0, lum: 0.0 }],
            shadow_tint: (0.0, 0.0, 0.003),
            highlight_tint: (0.0, 0.0, 0.0),
            chrome: 0.4,
            bw: false,
        },
    }
}

/// 색상환 거리 (0..180)
#[inline]
fn hue_dist(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(360.0);
    if d > 180.0 {
        360.0 - d
    } else {
        d
    }
}

// ---- 3D LUT --------------------------------------------------------------------

#[derive(Clone)]
pub struct Lut3d {
    pub size: usize,
    /// data[(r + g*size + b*size²) * 3] — r이 가장 빠르게 변함
    pub data: Vec<f32>,
}

impl Lut3d {
    /// 사면체(tetrahedral) 보간 — 삼선형보다 매끄러워 피부 그라데이션에서
    /// 층(밴딩)이 생기지 않는다 (영화/컬러 그레이딩 업계 표준).
    #[inline]
    pub fn sample(&self, r: f32, g: f32, b: f32) -> (f32, f32, f32) {
        let n = self.size;
        let max = (n - 1) as f32;
        let (rf, gf, bf) = (r.clamp(0.0, 1.0) * max, g.clamp(0.0, 1.0) * max, b.clamp(0.0, 1.0) * max);
        let (r0, g0, b0) = ((rf as usize).min(n - 2), (gf as usize).min(n - 2), (bf as usize).min(n - 2));
        let (fr, fg, fb) = (rf - r0 as f32, gf - g0 as f32, bf - b0 as f32);
        let at = |ri: usize, gi: usize, bi: usize| {
            let i = (ri + gi * n + bi * n * n) * 3;
            (self.data[i], self.data[i + 1], self.data[i + 2])
        };
        let add = |a: (f32, f32, f32), b: (f32, f32, f32), w: f32| {
            (a.0 + b.0 * w, a.1 + b.1 * w, a.2 + b.2 * w)
        };
        // 단위 큐브를 6개 사면체로 나눠 fr/fg/fb 크기 순서에 맞는 사면체의
        // 4개 꼭짓점만 가중 합산한다.
        let p000 = at(r0, g0, b0);
        let p111 = at(r0 + 1, g0 + 1, b0 + 1);
        let mut out = (0.0, 0.0, 0.0);
        if fr >= fg {
            if fg >= fb {
                // r ≥ g ≥ b
                out = add(out, p000, 1.0 - fr);
                out = add(out, at(r0 + 1, g0, b0), fr - fg);
                out = add(out, at(r0 + 1, g0 + 1, b0), fg - fb);
                out = add(out, p111, fb);
            } else if fr >= fb {
                // r ≥ b > g
                out = add(out, p000, 1.0 - fr);
                out = add(out, at(r0 + 1, g0, b0), fr - fb);
                out = add(out, at(r0 + 1, g0, b0 + 1), fb - fg);
                out = add(out, p111, fg);
            } else {
                // b > r ≥ g
                out = add(out, p000, 1.0 - fb);
                out = add(out, at(r0, g0, b0 + 1), fb - fr);
                out = add(out, at(r0 + 1, g0, b0 + 1), fr - fg);
                out = add(out, p111, fg);
            }
        } else if fb >= fg {
            // b ≥ g > r
            out = add(out, p000, 1.0 - fb);
            out = add(out, at(r0, g0, b0 + 1), fb - fg);
            out = add(out, at(r0, g0 + 1, b0 + 1), fg - fr);
            out = add(out, p111, fr);
        } else if fb >= fr {
            // g > b ≥ r
            out = add(out, p000, 1.0 - fg);
            out = add(out, at(r0, g0 + 1, b0), fg - fb);
            out = add(out, at(r0, g0 + 1, b0 + 1), fb - fr);
            out = add(out, p111, fr);
        } else {
            // g ≥ r > b
            out = add(out, p000, 1.0 - fg);
            out = add(out, at(r0, g0 + 1, b0), fg - fr);
            out = add(out, at(r0 + 1, g0 + 1, b0), fr - fb);
            out = add(out, p111, fb);
        }
        out
    }
}

/// .cube 파일 파싱 (LUT_3D_SIZE)
pub fn parse_cube(text: &str) -> Result<Lut3d, String> {
    let mut size = 0usize;
    let mut data: Vec<f32> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("TITLE") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("LUT_3D_SIZE") {
            size = rest.trim().parse().map_err(|_| "LUT_3D_SIZE 파싱 실패")?;
            data.reserve(size * size * size * 3);
            continue;
        }
        if line.starts_with("DOMAIN_") || line.starts_with("LUT_1D") {
            continue;
        }
        let mut it = line.split_whitespace();
        if let (Some(r), Some(g), Some(b)) = (it.next(), it.next(), it.next()) {
            if let (Ok(r), Ok(g), Ok(b)) = (r.parse::<f32>(), g.parse::<f32>(), b.parse::<f32>()) {
                data.push(r);
                data.push(g);
                data.push(b);
            }
        }
    }
    if size == 0 || data.len() != size * size * size * 3 {
        return Err(format!(
            ".cube 형식 오류 (size {size}, 값 {}개 — 기대 {}개)",
            data.len(),
            size * size * size * 3
        ));
    }
    Ok(Lut3d { size, data })
}

/// HaldCLUT PNG (정사각형, 변의 길이 = level³, 큐브 크기 = level²)
pub fn parse_hald(rgb: &[u8], w: u32, h: u32) -> Result<Lut3d, String> {
    if w != h {
        return Err("HaldCLUT은 정사각형이어야 합니다".into());
    }
    // 변 길이에서 level 찾기: side = level³
    let side = w as usize;
    let level = (side as f64).cbrt().round() as usize;
    if level * level * level != side {
        return Err("HaldCLUT 크기가 아닙니다 (변 길이 ≠ level³)".into());
    }
    let cube = level * level;
    let mut data = vec![0f32; cube * cube * cube * 3];
    // Hald 이미지의 픽셀 순서가 곧 (r 빠름 → g → b) 순서
    for (i, px) in rgb.chunks_exact(3).enumerate() {
        data[i * 3] = px[0] as f32 / 255.0;
        data[i * 3 + 1] = px[1] as f32 / 255.0;
        data[i * 3 + 2] = px[2] as f32 / 255.0;
    }
    Ok(Lut3d { size: cube, data })
}

/// 보정 전/후 이미지 쌍에서 3D LUT를 추출한다 (같은 사진·같은 크롭 가정).
/// 2패스: ① 전체 평균 → ② 평균에서 많이 벗어난 쌍(정렬 오차·라벨·노이즈)을
/// 버리고 다시 평균 — 물·하늘 같은 저채도 영역의 얼룩(뭉개짐)을 줄인다.
/// 샘플이 없는 격자점은 이웃 평균으로 채운 뒤 가볍게 스무딩한다.
#[allow(dead_code)] // 격자 직접 피팅 — 다항식(fit_smooth)으로 대체, 참고용 보존
pub fn fit_lut(src: &[u8], dst: &[u8], count: usize, size: usize) -> Lut3d {
    let n3 = size * size * size;
    let max = (size - 1) as f32;

    // 한 픽셀 쌍을 격자에 누적
    let splat = |acc: &mut [f64], wsum: &mut [f64], i: usize| {
        let sr = src[i * 3] as f32 / 255.0 * max;
        let sg = src[i * 3 + 1] as f32 / 255.0 * max;
        let sb = src[i * 3 + 2] as f32 / 255.0 * max;
        let (r0, g0, b0) = (sr as usize, sg as usize, sb as usize);
        let (r1, g1, b1) = ((r0 + 1).min(size - 1), (g0 + 1).min(size - 1), (b0 + 1).min(size - 1));
        let (fr, fg, fb) = (sr - r0 as f32, sg - g0 as f32, sb - b0 as f32);
        let dr = dst[i * 3] as f64 / 255.0;
        let dg = dst[i * 3 + 1] as f64 / 255.0;
        let db = dst[i * 3 + 2] as f64 / 255.0;
        for (ri, wr) in [(r0, 1.0 - fr), (r1, fr)] {
            for (gi, wg) in [(g0, 1.0 - fg), (g1, fg)] {
                for (bi, wb) in [(b0, 1.0 - fb), (b1, fb)] {
                    let wgt = (wr * wg * wb) as f64;
                    if wgt <= 0.0 {
                        continue;
                    }
                    let idx = ri + gi * size + bi * size * size;
                    acc[idx * 3] += dr * wgt;
                    acc[idx * 3 + 1] += dg * wgt;
                    acc[idx * 3 + 2] += db * wgt;
                    wsum[idx] += wgt;
                }
            }
        }
    };
    // 쌍이 속한 가장 가까운 격자점
    let nearest = |i: usize| -> usize {
        let r = (src[i * 3] as f32 / 255.0 * max).round() as usize;
        let g = (src[i * 3 + 1] as f32 / 255.0 * max).round() as usize;
        let b = (src[i * 3 + 2] as f32 / 255.0 * max).round() as usize;
        r + g * size + b * size * size
    };

    // ---- 1패스: 전체 평균 ----
    let mut acc = vec![0f64; n3 * 3];
    let mut wsum = vec![0f64; n3];
    for i in 0..count {
        splat(&mut acc, &mut wsum, i);
    }
    let mut mean1 = vec![0f32; n3 * 3];
    for c in 0..n3 {
        if wsum[c] > 1e-3 {
            mean1[c * 3] = (acc[c * 3] / wsum[c]) as f32;
            mean1[c * 3 + 1] = (acc[c * 3 + 1] / wsum[c]) as f32;
            mean1[c * 3 + 2] = (acc[c * 3 + 2] / wsum[c]) as f32;
        }
    }

    // ---- 2패스: 1패스 평균에서 먼 쌍은 이상치로 버리고 다시 평균 ----
    const REJECT: f32 = 0.10; // RGB 거리 임계값
    let mut acc2 = vec![0f64; n3 * 3];
    let mut wsum2 = vec![0f64; n3];
    for i in 0..count {
        let cell = nearest(i);
        if wsum[cell] > 1e-3 {
            let dr = dst[i * 3] as f32 / 255.0 - mean1[cell * 3];
            let dg = dst[i * 3 + 1] as f32 / 255.0 - mean1[cell * 3 + 1];
            let db = dst[i * 3 + 2] as f32 / 255.0 - mean1[cell * 3 + 2];
            if (dr * dr + dg * dg + db * db).sqrt() > REJECT {
                continue; // 이상치 (정렬 오차·라벨·노이즈)
            }
        }
        splat(&mut acc2, &mut wsum2, i);
    }

    let mut data = vec![0f32; n3 * 3];
    let mut filled = vec![false; n3];
    for c in 0..n3 {
        if wsum2[c] > 1e-3 {
            data[c * 3] = (acc2[c * 3] / wsum2[c]) as f32;
            data[c * 3 + 1] = (acc2[c * 3 + 1] / wsum2[c]) as f32;
            data[c * 3 + 2] = (acc2[c * 3 + 2] / wsum2[c]) as f32;
            filled[c] = true;
        } else if wsum[c] > 1e-3 {
            // 2패스에서 전부 거부됐으면 1패스 평균이라도 사용
            data[c * 3] = mean1[c * 3];
            data[c * 3 + 1] = mean1[c * 3 + 1];
            data[c * 3 + 2] = mean1[c * 3 + 2];
            filled[c] = true;
        }
    }

    // 실측 여부 기록
    let measured = filled.clone();

    // ---- 델타 공간으로 변환 ------------------------------------------------------
    // 절대색 대신 (출력 − 입력) 오프셋을 저장한다. 미측정 영역을 절대색으로 채우면
    // 서로 다른 입력이 같은 출력으로 수렴해 질감(디테일)이 뭉개진다 — 델타로 채우면
    // "이웃과 같은 방향으로 이동"만 물려받고 입력 간 차이는 그대로 보존된다.
    let ident = |idx: usize| -> (f32, f32, f32) {
        (
            (idx % size) as f32 / max,
            ((idx / size) % size) as f32 / max,
            (idx / (size * size)) as f32 / max,
        )
    };
    let mut delta = vec![0f32; n3 * 3];
    for idx in 0..n3 {
        if measured[idx] {
            let (ir, ig, ib) = ident(idx);
            delta[idx * 3] = data[idx * 3] - ir;
            delta[idx * 3 + 1] = data[idx * 3 + 1] - ig;
            delta[idx * 3 + 2] = data[idx * 3 + 2] - ib;
        }
    }

    // 빈 격자점의 델타를 이웃 평균으로 반복 전파
    let neighbors = |idx: usize| -> [Option<usize>; 6] {
        let r = idx % size;
        let g = (idx / size) % size;
        let b = idx / (size * size);
        [
            (r > 0).then(|| idx - 1),
            (r + 1 < size).then(|| idx + 1),
            (g > 0).then(|| idx - size),
            (g + 1 < size).then(|| idx + size),
            (b > 0).then(|| idx - size * size),
            (b + 1 < size).then(|| idx + size * size),
        ]
    };
    for _ in 0..size * 3 {
        let mut progressed = false;
        let snapshot = filled.clone();
        for idx in 0..n3 {
            if snapshot[idx] {
                continue;
            }
            let mut sum = [0f32; 3];
            let mut cnt = 0;
            for nb in neighbors(idx).into_iter().flatten() {
                if snapshot[nb] {
                    sum[0] += delta[nb * 3];
                    sum[1] += delta[nb * 3 + 1];
                    sum[2] += delta[nb * 3 + 2];
                    cnt += 1;
                }
            }
            if cnt > 0 {
                delta[idx * 3] = sum[0] / cnt as f32;
                delta[idx * 3 + 1] = sum[1] / cnt as f32;
                delta[idx * 3 + 2] = sum[2] / cnt as f32;
                filled[idx] = true;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    // (채워지지 않은 셀은 델타 0 = 항등)

    // 델타 스무딩 — 격자 노이즈 제거. 델타를 스무딩하면 절대색과 달리
    // 입력 간 기울기(질감)를 누르지 않는다. 실측 90/10, 추정 50/50.
    let src_d = delta.clone();
    for idx in 0..n3 {
        let mut sum = [0f32; 3];
        let mut cnt = 0;
        for nb in neighbors(idx).into_iter().flatten() {
            sum[0] += src_d[nb * 3];
            sum[1] += src_d[nb * 3 + 1];
            sum[2] += src_d[nb * 3 + 2];
            cnt += 1;
        }
        if cnt > 0 {
            let keep = if measured[idx] { 0.9 } else { 0.5 };
            for c in 0..3 {
                delta[idx * 3 + c] = src_d[idx * 3 + c] * keep + sum[c] / cnt as f32 * (1.0 - keep);
            }
        }
    }

    // 최종: 항등 + 델타, 색역을 벗어나면 밝기를 보존한 채 채도만 압축
    // (하드 클리핑하면 진한 빨강(김치 고춧가루)의 질감이 평평하게 날아간다)
    for idx in 0..n3 {
        let (ir, ig, ib) = ident(idx);
        let (r, g, b) = fit_gamut(ir + delta[idx * 3], ig + delta[idx * 3 + 1], ib + delta[idx * 3 + 2]);
        data[idx * 3] = r;
        data[idx * 3 + 1] = g;
        data[idx * 3 + 2] = b;
    }

    Lut3d { size, data }
}

// ---- 3차 다항식 색 변환 피팅 ------------------------------------------------------
// 격자 LUT를 픽셀 쌍으로 직접 채우면(fit_lut) 미측정 영역을 이웃에서 메꾸며
// 생기는 셀 노이즈가 피부 밴딩·중립색 오염(간장 그릇)의 근본 원인이 된다.
// 대신 매끈함이 구조적으로 보장되는 3차 다항식 f: RGB→RGB를 최소자승으로 맞추고,
// 그 함수를 샘플링해 LUT를 만든다 — 불연속이 아예 없어 밴딩이 생길 수 없다.

/// 3차 다항식 기저 (20항)
#[inline]
fn poly_basis(r: f32, g: f32, b: f32) -> [f32; 20] {
    [
        1.0,
        r,
        g,
        b,
        r * r,
        g * g,
        b * b,
        r * g,
        r * b,
        g * b,
        r * r * r,
        g * g * g,
        b * b * b,
        r * r * g,
        r * r * b,
        g * g * r,
        g * g * b,
        b * b * r,
        b * b * g,
        r * g * b,
    ]
}

/// 가우스 소거(부분 피벗)로 20×20 선형계 3개(채널별)를 푼다.
fn solve_normal(a: &[[f64; 20]; 20], rhs: &[[f64; 20]; 3]) -> Option<[[f32; 20]; 3]> {
    // 증강행렬 [A | rhs_r rhs_g rhs_b]
    let mut m = [[0f64; 23]; 20];
    for i in 0..20 {
        for j in 0..20 {
            m[i][j] = a[i][j];
        }
        for c in 0..3 {
            m[i][20 + c] = rhs[c][i];
        }
    }
    for col in 0..20 {
        // 피벗
        let mut piv = col;
        for row in col + 1..20 {
            if m[row][col].abs() > m[piv][col].abs() {
                piv = row;
            }
        }
        if m[piv][col].abs() < 1e-12 {
            return None;
        }
        m.swap(col, piv);
        let d = m[col][col];
        for j in col..23 {
            m[col][j] /= d;
        }
        for row in 0..20 {
            if row != col {
                let f = m[row][col];
                if f != 0.0 {
                    for j in col..23 {
                        m[row][j] -= f * m[col][j];
                    }
                }
            }
        }
    }
    let mut out = [[0f32; 20]; 3];
    for c in 0..3 {
        for i in 0..20 {
            out[c][i] = m[i][20 + c] as f32;
        }
    }
    Some(out)
}

/// 픽셀 쌍 (src→dst, u8 RGB)에 3차 다항식을 최소자승 피팅.
/// use_mask가 Some이면 true인 쌍만 사용 (이상치 제거 2차 피팅용).
fn fit_poly(src: &[u8], dst: &[u8], count: usize, use_mask: Option<&[bool]>) -> Option<[[f32; 20]; 3]> {
    let mut a = [[0f64; 20]; 20];
    let mut rhs = [[0f64; 20]; 3];
    for i in 0..count {
        if let Some(mask) = use_mask {
            if !mask[i] {
                continue;
            }
        }
        let phi = poly_basis(
            src[i * 3] as f32 / 255.0,
            src[i * 3 + 1] as f32 / 255.0,
            src[i * 3 + 2] as f32 / 255.0,
        );
        let d = [
            dst[i * 3] as f64 / 255.0,
            dst[i * 3 + 1] as f64 / 255.0,
            dst[i * 3 + 2] as f64 / 255.0,
        ];
        for p in 0..20 {
            let fp = phi[p] as f64;
            for q in p..20 {
                a[p][q] += fp * phi[q] as f64;
            }
            for c in 0..3 {
                rhs[c][p] += fp * d[c];
            }
        }
    }
    // 대칭 보완
    for p in 0..20 {
        for q in 0..p {
            a[p][q] = a[q][p];
        }
    }
    solve_normal(&a, &rhs)
}

#[inline]
fn poly_eval(coef: &[[f32; 20]; 3], r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let phi = poly_basis(r, g, b);
    let mut out = [0f32; 3];
    for (c, oc) in out.iter_mut().enumerate() {
        let mut s = 0f32;
        for p in 0..20 {
            s += coef[c][p] * phi[p];
        }
        *oc = s;
    }
    (out[0], out[1], out[2])
}

/// 다항식을 샘플링해 LUT 생성 (색역은 밝기 보존 압축)
fn poly_to_lut(coef: &[[f32; 20]; 3], size: usize) -> Lut3d {
    let max = (size - 1) as f32;
    let mut data = vec![0f32; size * size * size * 3];
    for bi in 0..size {
        for gi in 0..size {
            for ri in 0..size {
                let (r, g, b) = poly_eval(coef, ri as f32 / max, gi as f32 / max, bi as f32 / max);
                let (r, g, b) = fit_gamut(r, g, b);
                let idx = (ri + gi * size + bi * size * size) * 3;
                data[idx] = r;
                data[idx + 1] = g;
                data[idx + 2] = b;
            }
        }
    }
    Lut3d { size, data }
}

/// 전/후 픽셀 쌍 → 매끈한 변환 LUT (3차 다항식, 이상치 제거 2패스)
pub fn fit_smooth(src: &[u8], dst: &[u8], count: usize) -> Result<Lut3d, String> {
    let coef1 = fit_poly(src, dst, count, None).ok_or("피팅 실패 (샘플 부족)")?;
    // 1차 피팅 잔차가 큰 쌍(정렬 오차·라벨·노이즈)을 버리고 다시 피팅
    let mut mask = vec![true; count];
    for i in 0..count {
        let (pr, pg, pb) = poly_eval(
            &coef1,
            src[i * 3] as f32 / 255.0,
            src[i * 3 + 1] as f32 / 255.0,
            src[i * 3 + 2] as f32 / 255.0,
        );
        let dr = pr - dst[i * 3] as f32 / 255.0;
        let dg = pg - dst[i * 3 + 1] as f32 / 255.0;
        let db = pb - dst[i * 3 + 2] as f32 / 255.0;
        if (dr * dr + dg * dg + db * db).sqrt() > 0.09 {
            mask[i] = false;
        }
    }
    let coef = fit_poly(src, dst, count, Some(&mask)).unwrap_or(coef1);
    Ok(poly_to_lut(&coef, 33))
}

/// 기존(노이즈 있는) LUT를 다항식으로 리핏해 매끈한 LUT로 재생성 —
/// 옛 추출 방식으로 구운 임베드 LUT(Fuji_Style)의 밴딩·오염 제거용.
pub fn refit_lut(lut: &Lut3d) -> Lut3d {
    let n3 = lut.size * lut.size * lut.size;
    let max = (lut.size - 1) as f32;
    let mut src = vec![0u8; n3 * 3];
    let mut dst = vec![0u8; n3 * 3];
    for idx in 0..n3 {
        src[idx * 3] = ((idx % lut.size) as f32 / max * 255.0) as u8;
        src[idx * 3 + 1] = (((idx / lut.size) % lut.size) as f32 / max * 255.0) as u8;
        src[idx * 3 + 2] = ((idx / (lut.size * lut.size)) as f32 / max * 255.0) as u8;
        for c in 0..3 {
            dst[idx * 3 + c] = (lut.data[idx * 3 + c] * 255.0 + 0.5) as u8;
        }
    }
    fit_smooth(&src, &dst, n3).unwrap_or_else(|_| lut.clone())
}

/// LUT 스무딩 (델타 공간, 다중 패스) — 이웃 격자점 간 값이 들쭉날쭉하면
/// 피부처럼 매끈한 그라데이션에서 등고선(밴딩)이 생긴다. 입력-출력 오프셋을
/// 6-이웃 평균과 혼합해 매핑을 매끈하게 만든다. (색 디테일 손실은
/// calibrate_lut가, 공간 디테일은 디테일 재주입이 보상한다)
pub fn smooth_lut(lut: &mut Lut3d, passes: usize, keep: f32) {
    let size = lut.size;
    let n3 = size * size * size;
    let max = (size - 1) as f32;
    let ident = |idx: usize| -> (f32, f32, f32) {
        (
            (idx % size) as f32 / max,
            ((idx / size) % size) as f32 / max,
            (idx / (size * size)) as f32 / max,
        )
    };
    let mut delta = vec![0f32; n3 * 3];
    for idx in 0..n3 {
        let (ir, ig, ib) = ident(idx);
        delta[idx * 3] = lut.data[idx * 3] - ir;
        delta[idx * 3 + 1] = lut.data[idx * 3 + 1] - ig;
        delta[idx * 3 + 2] = lut.data[idx * 3 + 2] - ib;
    }
    for _ in 0..passes {
        let src = delta.clone();
        for idx in 0..n3 {
            let r = idx % size;
            let g = (idx / size) % size;
            let b = idx / (size * size);
            let mut sum = [0f32; 3];
            let mut cnt = 0;
            let mut acc = |nb: usize| {
                sum[0] += src[nb * 3];
                sum[1] += src[nb * 3 + 1];
                sum[2] += src[nb * 3 + 2];
                cnt += 1;
            };
            if r > 0 {
                acc(idx - 1);
            }
            if r + 1 < size {
                acc(idx + 1);
            }
            if g > 0 {
                acc(idx - size);
            }
            if g + 1 < size {
                acc(idx + size);
            }
            if b > 0 {
                acc(idx - size * size);
            }
            if b + 1 < size {
                acc(idx + size * size);
            }
            if cnt > 0 {
                for c in 0..3 {
                    delta[idx * 3 + c] = src[idx * 3 + c] * keep + sum[c] / cnt as f32 * (1.0 - keep);
                }
            }
        }
    }
    for idx in 0..n3 {
        let (ir, ig, ib) = ident(idx);
        let (r, g, b) = fit_gamut(ir + delta[idx * 3], ig + delta[idx * 3 + 1], ib + delta[idx * 3 + 2]);
        lut.data[idx * 3] = r;
        lut.data[idx * 3 + 1] = g;
        lut.data[idx * 3 + 2] = b;
    }
}

/// 색역 압축 — [0,1]을 벗어난 색을 밝기는 유지한 채 채도만 줄여 맞춘다.
#[inline]
fn fit_gamut(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let l = luma(r, g, b).clamp(0.0, 1.0);
    let (dr, dg, db) = (r - l, g - l, b - l);
    let mut t = 1f32;
    for d in [dr, dg, db] {
        if l + d > 1.0 && d > 1e-6 {
            t = t.min((1.0 - l) / d);
        } else if l + d < 0.0 && d < -1e-6 {
            t = t.min(l / -d);
        }
    }
    let t = t.clamp(0.0, 1.0);
    (l + dr * t, l + dg * t, l + db * t)
}

/// 추출된 LUT 자동 캘리브레이션 — 격자 평균 때문에 뭉개진 대비·채도를
/// 목표(after) 사진의 통계에 맞춰 복원한다.
/// ① src를 LUT로 변환한 결과와 dst의 밝기 평균/표준편차를 맞추고(대비),
/// ② 채도(크로마) 평균 비율만큼 LUT 전체의 크로마를 키운다(진하게).
#[allow(dead_code)] // 격자 피팅용 보정 — 다항식 전환으로 미사용, 참고용 보존
pub fn calibrate_lut(lut: &mut Lut3d, src: &[u8], dst: &[u8], count: usize) {
    let mut m_l = 0f64; // 매핑 결과 밝기 합
    let mut m_l2 = 0f64;
    let mut m_c = 0f64; // 매핑 결과 크로마 합
    let mut d_l = 0f64;
    let mut d_l2 = 0f64;
    let mut d_c = 0f64;
    for i in 0..count {
        let (r, g, b) = lut.sample(
            src[i * 3] as f32 / 255.0,
            src[i * 3 + 1] as f32 / 255.0,
            src[i * 3 + 2] as f32 / 255.0,
        );
        let l = luma(r, g, b) as f64;
        let c = (r.max(g).max(b) - r.min(g).min(b)) as f64;
        m_l += l;
        m_l2 += l * l;
        m_c += c;

        let (dr, dg, db) = (
            dst[i * 3] as f32 / 255.0,
            dst[i * 3 + 1] as f32 / 255.0,
            dst[i * 3 + 2] as f32 / 255.0,
        );
        let l = luma(dr, dg, db) as f64;
        let c = (dr.max(dg).max(db) - dr.min(dg).min(db)) as f64;
        d_l += l;
        d_l2 += l * l;
        d_c += c;
    }
    let n = count as f64;
    let (m_mean, d_mean) = (m_l / n, d_l / n);
    let m_std = (m_l2 / n - m_mean * m_mean).max(1e-6).sqrt();
    let d_std = (d_l2 / n - d_mean * d_mean).max(1e-6).sqrt();
    // 대비 (밝기 분포 매칭): l' = a*l + b
    let a = ((d_std / m_std) as f32).clamp(0.85, 1.3);
    let b = (d_mean - (a as f64) * m_mean) as f32;
    // 채도 (크로마 평균 매칭)
    let gain_c = ((d_c / m_c.max(1e-9)) as f32).clamp(0.9, 1.35);

    for i in 0..lut.size.pow(3) {
        let (r, g, b_) = (lut.data[i * 3], lut.data[i * 3 + 1], lut.data[i * 3 + 2]);
        let l = luma(r, g, b_);
        let nl = (a * l + b).clamp(0.0, 1.0);
        // 채도 가중: 원래 쨍한 색만 증폭하고, 저채도(피부·이빨·물·하늘)는 그대로 둔다.
        // 일괄 증폭하면 중립색의 미세한 색 끼·압축 노이즈까지 커져서
        // 이빨이 변색되고 물이 얼룩져 해상도가 떨어져 보인다.
        let c = r.max(g).max(b_) - r.min(g).min(b_);
        let t = ((c - 0.06) / 0.20).clamp(0.0, 1.0);
        let t = t * t * (3.0 - 2.0 * t);
        let g_w = 1.0 + (gain_c - 1.0) * t;
        // 하드 클리핑 대신 색역 압축 — 진한 색의 질감(채널 간 변화)을 보존
        let (nr, ng, nb) = fit_gamut(nl + (r - l) * g_w, nl + (g - l) * g_w, nl + (b_ - l) * g_w);
        lut.data[i * 3] = nr;
        lut.data[i * 3 + 1] = ng;
        lut.data[i * 3 + 2] = nb;
    }
}

/// LUT를 .cube 텍스트로 직렬화
pub fn to_cube(lut: &Lut3d, title: &str) -> String {
    let mut out = String::with_capacity(lut.size.pow(3) * 24);
    out.push_str(&format!("TITLE \"{title}\"\nLUT_3D_SIZE {}\n", lut.size));
    for i in 0..lut.size.pow(3) {
        out.push_str(&format!(
            "{:.6} {:.6} {:.6}\n",
            lut.data[i * 3],
            lut.data[i * 3 + 1],
            lut.data[i * 3 + 2]
        ));
    }
    out
}

// ---- 유틸 ------------------------------------------------------------------------

/// 사용자 대비 (완만한 S커브 혼합)
#[inline]
fn user_contrast(x: f32, amt: f32) -> f32 {
    if amt >= 0.0 {
        let s = x * x * (3.0 - 2.0 * x);
        x + (s - x) * amt
    } else {
        let flat = 0.5 + (x - 0.5) * 0.65;
        x + (flat - x) * (-amt)
    }
}

/// 분리형 박스 블러 (가로+세로, 프리픽스 합 O(n)) — 디테일 재주입용 저주파 추출
fn box_blur(src: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut tmp = vec![0f32; w * h];
    let mut out = vec![0f32; w * h];
    let mut prefix = vec![0f32; w.max(h) + 1];
    // 가로
    for y in 0..h {
        let row = &src[y * w..y * w + w];
        for x in 0..w {
            prefix[x + 1] = prefix[x] + row[x];
        }
        for x in 0..w {
            let lo = x.saturating_sub(r);
            let hi = (x + r + 1).min(w);
            tmp[y * w + x] = (prefix[hi] - prefix[lo]) / (hi - lo) as f32;
        }
    }
    // 세로
    for x in 0..w {
        for y in 0..h {
            prefix[y + 1] = prefix[y] + tmp[y * w + x];
        }
        for y in 0..h {
            let lo = y.saturating_sub(r);
            let hi = (y + r + 1).min(h);
            out[y * w + x] = (prefix[hi] - prefix[lo]) / (hi - lo) as f32;
        }
    }
    out
}

/// 피부색 확률 (0..1) — YCbCr 범위 기반. 입력은 0..1 RGB.
/// 채도 상한 예외(얼굴 밴딩 방지)와 피부 보정 마스크에 공용.
#[inline]
fn skin_prob(r: f32, g: f32, b: f32) -> f32 {
    let (r, g, b) = (r * 255.0, g * 255.0, b * 255.0);
    let cb = 128.0 - 0.168_736 * r - 0.331_264 * g + 0.5 * b;
    let cr = 128.0 + 0.5 * r - 0.418_688 * g - 0.081_312 * b;
    let mb = 1.0 - ((cb - 102.0).abs() / 25.0).clamp(0.0, 1.0);
    let mr = 1.0 - ((cr - 153.0).abs() / 20.0).clamp(0.0, 1.0);
    let m = (mb * mr).clamp(0.0, 1.0);
    m * m * (3.0 - 2.0 * m)
}

#[inline]
fn hash_noise(x: u32, y: u32, seed: u32) -> f32 {
    let mut h = x
        .wrapping_mul(374_761_393)
        .wrapping_add(y.wrapping_mul(668_265_263))
        .wrapping_add(seed.wrapping_mul(2_246_822_519));
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    (h as f32 / u32::MAX as f32) * 2.0 - 1.0
}

// ---- 메인 파이프라인 -------------------------------------------------------------

pub fn apply(rgb: &[u8], w: u32, h: u32, p: &Params, lut: Option<&Lut3d>) -> Vec<u8> {
    let (w_us, h_us) = (w as usize, h as usize);
    let n = w_us * h_us;
    let mut buf = vec![0f32; n * 3];

    let rec = recipe(&p.film);
    let tone = curve_lut(rec.curve);
    // "lut"(사용자 로드/추출) 및 내장 LUT 필름("fuji_style" 등)은 전달된 LUT로 처리
    let use_lut = lut.is_some() && matches!(p.film.as_str(), "lut" | "fuji_style");

    // 화이트밸런스: 사용자 색온도 × 필름 자체 WB
    let t = p.temperature.clamp(-1.0, 1.0);
    let wr = (1.0 + 0.18 * t) * if use_lut { 1.0 } else { rec.wb.0 };
    let wb_b = (1.0 - 0.18 * t) * if use_lut { 1.0 } else { rec.wb.1 };
    let ev = p.exposure.clamp(-1.0, 1.0).exp2();
    let user_sat = 1.0 + p.saturation.clamp(-1.0, 1.0);
    let contrast = p.contrast.clamp(-1.0, 1.0);

    for i in 0..n {
        let mut r = clamp01(rgb[i * 3] as f32 / 255.0 * wr * ev);
        let mut g = clamp01(rgb[i * 3 + 1] as f32 / 255.0 * ev);
        let mut b = clamp01(rgb[i * 3 + 2] as f32 / 255.0 * wb_b * ev);

        if use_lut {
            // LUT가 다항식 기반으로 매끈하게 생성되므로(fit_smooth/refit_lut)
            // 채도 상한·중립 보호 같은 사후 패치 없이 그대로 샘플링한다.
            let (lr, lg, lb) = lut.unwrap().sample(r, g, b);
            r = clamp01(lr);
            g = clamp01(lg);
            b = clamp01(lb);
        } else if rec.bw {
            // 흑백 (붉은 감도 가중) → 톤커브
            let gy = clamp01(0.30 * r + 0.59 * g + 0.11 * b);
            let v = tone[(gy * 255.0) as usize];
            r = v;
            g = v;
            b = v;
        } else {
            // 1) 필름 톤커브 (채널별)
            r = tone[(r * 255.0) as usize];
            g = tone[(g * 255.0) as usize];
            b = tone[(b * 255.0) as usize];

            // 2) 색상별 선택 조정 + 전체 채도 (HSL에서)
            let (mut hh, mut ss, mut ll) = rgb_to_hsl(r, g, b);
            for adj in rec.hues {
                let d = hue_dist(hh, adj.center);
                let wgt = (-0.5 * (d / adj.sigma) * (d / adj.sigma)).exp() * ss.min(1.0);
                ss *= 1.0 + (adj.sat - 1.0) * wgt;
                hh += adj.hue * wgt;
                ll += adj.lum * wgt;
            }
            ss = clamp01(ss * rec.sat);
            ll = clamp01(ll);

            // 3) 컬러 크롬: 밝고 채도 높은 곳 밝기 누르기
            if rec.chrome > 0.0 {
                let wgt = ss * ((ll - 0.55) / 0.45).clamp(0.0, 1.0);
                ll -= rec.chrome * 0.07 * wgt;
            }
            let (nr, ng, nb) = hsl_to_rgb(hh, ss, clamp01(ll));
            r = nr;
            g = ng;
            b = nb;

            // 4) 스플릿 토닝
            let l = luma(r, g, b);
            let ws = (1.0 - l) * (1.0 - l);
            let wh = l * l;
            r = clamp01(r + rec.shadow_tint.0 * ws + rec.highlight_tint.0 * wh);
            g = clamp01(g + rec.shadow_tint.1 * ws + rec.highlight_tint.1 * wh);
            b = clamp01(b + rec.shadow_tint.2 * ws + rec.highlight_tint.2 * wh);
        }

        // 사용자 대비·채도
        r = user_contrast(clamp01(r), contrast);
        g = user_contrast(clamp01(g), contrast);
        b = user_contrast(clamp01(b), contrast);
        if (user_sat - 1.0).abs() > 0.001 && !rec.bw {
            let l = luma(r, g, b);
            r = clamp01(l + (r - l) * user_sat);
            g = clamp01(l + (g - l) * user_sat);
            b = clamp01(l + (b - l) * user_sat);
        }

        buf[i * 3] = r;
        buf[i * 3 + 1] = g;
        buf[i * 3 + 2] = b;
    }

    // ---- 디테일 재주입 (RGB 채널별) -------------------------------------------------
    // 색 변환(특히 LUT)이 비슷한 색들을 한 색으로 모으면 미세 질감이 사라지고,
    // 매끈한 그라데이션(피부)에는 등고선(밴딩)이 생긴다. 채널별로 원본의 고주파와
    // 결과물의 고주파를 비교해 잃어버린 만큼 되살린다 — 밝기 질감뿐 아니라
    // 색 등고선의 경계도 원본의 매끈함으로 상쇄된다.
    {
        let k = if use_lut { 0.9 } else { 0.35 }; // 내장 레시피는 의도된 소프트함을 존중
        // 이미지 크기에 비례한 반경 (미리보기/원본 저장에서 같은 체감)
        let radius = ((w_us.min(h_us) / 400).max(1)).min(8);
        let mut c0 = vec![0f32; n];
        let mut c1 = vec![0f32; n];
        for ch in 0..3 {
            for i in 0..n {
                c0[i] = rgb[i * 3 + ch] as f32 / 255.0;
                c1[i] = buf[i * 3 + ch];
            }
            let b0 = box_blur(&c0, w_us, h_us, radius);
            let b1 = box_blur(&c1, w_us, h_us, radius);
            for i in 0..n {
                // 원본 고주파 − 결과 고주파 = 변환으로 사라진 디테일
                let d = ((c0[i] - b0[i]) - (c1[i] - b1[i])) * k;
                if d.abs() > 0.0005 {
                    buf[i * 3 + ch] = clamp01(buf[i * 3 + ch] + d);
                }
            }
        }
    }

    // ---- 피부 보정 (잡티·톤 얼룩 스무딩) --------------------------------------------
    // 원본 색으로 피부색 확률 마스크(YCbCr)를 만들고, 마스크 안에서
    // 작은 변화(잡티·톤 얼룩)만 블러로 스무딩하고 큰 변화(눈·입·윤곽)는 유지한다.
    if p.skin > 0.005 {
        let strength = p.skin.clamp(0.0, 1.0);
        let r2 = ((w_us.min(h_us) / 160).max(2)).min(12);
        // 피부 마스크 (원본 기준 — 필터로 색이 변해도 마스크는 안정적)
        let mut mask = vec![0f32; n];
        for i in 0..n {
            mask[i] = skin_prob(
                rgb[i * 3] as f32 / 255.0,
                rgb[i * 3 + 1] as f32 / 255.0,
                rgb[i * 3 + 2] as f32 / 255.0,
            );
        }
        // 마스크 자체도 부드럽게 (경계 티 안 나게)
        let mask = box_blur(&mask, w_us, h_us, r2);
        // 채널별 저주파 + "작은 변화만 눌러주기"
        let mut ch = vec![0f32; n];
        for c in 0..3 {
            for i in 0..n {
                ch[i] = buf[i * 3 + c];
            }
            let low = box_blur(&ch, w_us, h_us, r2);
            for i in 0..n {
                let m = mask[i] * strength;
                if m < 0.01 {
                    continue;
                }
                let d = ch[i] - low[i];
                // |d|가 작으면(잡티) 거의 제거, 크면(윤곽) 유지
                let f = (d.abs() / 0.10).clamp(0.0, 1.0);
                let f = f * f * (3.0 - 2.0 * f);
                let smoothed = low[i] + d * f;
                buf[i * 3 + c] = clamp01(ch[i] + (smoothed - ch[i]) * m);
            }
        }
    }

    // ---- 샤픈 (언샵 마스크) ----
    if p.sharpen > 0.005 && w_us > 2 && h_us > 2 {
        let k = p.sharpen.clamp(0.0, 1.0) * 1.3;
        let src = buf.clone();
        for y in 1..h_us - 1 {
            for x in 1..w_us - 1 {
                for c in 0..3 {
                    let idx = (y * w_us + x) * 3 + c;
                    let mut sum = 0f32;
                    for dy in 0..3 {
                        for dx in 0..3 {
                            sum += src[((y + dy - 1) * w_us + (x + dx - 1)) * 3 + c];
                        }
                    }
                    let avg = sum / 9.0;
                    buf[idx] = clamp01(src[idx] + (src[idx] - avg) * k);
                }
            }
        }
    }

    // ---- 그레인(2옥타브, 뭉친 입자) + 비네트 ----
    let grain = p.grain.clamp(0.0, 1.0);
    let vig = p.vignette.clamp(0.0, 1.0);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let max_d = (cx * cx + cy * cy).sqrt();
    for y in 0..h_us {
        for x in 0..w_us {
            let i = (y * w_us + x) * 3;
            let mut r = buf[i];
            let mut g = buf[i + 1];
            let mut b = buf[i + 2];

            if grain > 0.005 {
                let l = luma(r, g, b);
                let amp = grain * 0.09 * (1.0 - (2.0 * l - 1.0).abs() * 0.55);
                // 거친 입자(저주파) + 미세 입자(고주파)를 섞어 필름처럼 뭉치게
                let nz = 0.6 * hash_noise(x as u32 / 2, y as u32 / 2, 7) + 0.4 * hash_noise(x as u32, y as u32, 13);
                r += nz * amp;
                g += nz * amp;
                b += nz * amp;
            }
            if vig > 0.005 {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let d = (dx * dx + dy * dy).sqrt() / max_d;
                let fall = ((d - 0.45) / 0.55).clamp(0.0, 1.0);
                let f = 1.0 - vig * 0.55 * fall * fall;
                r *= f;
                g *= f;
                b *= f;
            }
            buf[i] = clamp01(r);
            buf[i + 1] = clamp01(g);
            buf[i + 2] = clamp01(b);
        }
    }

    // f32 → u8 (+디더링): 8비트 양자화가 강한 톤커브로 증폭되며 생기는
    // 피부·하늘의 층(밴딩)을 ±0.5 LSB 노이즈로 흩어서 없앤다 (눈에는 안 보임)
    let mut out = vec![0u8; n * 3];
    for y in 0..h_us {
        for x in 0..w_us {
            let i = (y * w_us + x) * 3;
            let nz = hash_noise(x as u32, y as u32, 101) * 0.9;
            for c in 0..3 {
                out[i + c] = (buf[i + c] * 255.0 + 0.5 + nz).clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}
