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

pub struct Lut3d {
    pub size: usize,
    /// data[(r + g*size + b*size²) * 3] — r이 가장 빠르게 변함
    pub data: Vec<f32>,
}

impl Lut3d {
    /// 삼선형 보간 적용
    #[inline]
    pub fn sample(&self, r: f32, g: f32, b: f32) -> (f32, f32, f32) {
        let n = self.size;
        let max = (n - 1) as f32;
        let (rf, gf, bf) = (r * max, g * max, b * max);
        let (r0, g0, b0) = (rf as usize, gf as usize, bf as usize);
        let (r1, g1, b1) = ((r0 + 1).min(n - 1), (g0 + 1).min(n - 1), (b0 + 1).min(n - 1));
        let (fr, fg, fb) = (rf - r0 as f32, gf - g0 as f32, bf - b0 as f32);
        let at = |ri: usize, gi: usize, bi: usize| {
            let i = (ri + gi * n + bi * n * n) * 3;
            (self.data[i], self.data[i + 1], self.data[i + 2])
        };
        let lerp = |a: (f32, f32, f32), b: (f32, f32, f32), t: f32| {
            (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
        };
        let c00 = lerp(at(r0, g0, b0), at(r1, g0, b0), fr);
        let c10 = lerp(at(r0, g1, b0), at(r1, g1, b0), fr);
        let c01 = lerp(at(r0, g0, b1), at(r1, g0, b1), fr);
        let c11 = lerp(at(r0, g1, b1), at(r1, g1, b1), fr);
        let c0 = lerp(c00, c10, fg);
        let c1 = lerp(c01, c11, fg);
        lerp(c0, c1, fb)
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

    // 실측 여부 기록 — 스무딩은 추정(채움) 격자점에만 적용해 실측 색을 보존한다
    let measured = filled.clone();

    // 빈 격자점을 이웃 평균으로 반복 전파
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
                    sum[0] += data[nb * 3];
                    sum[1] += data[nb * 3 + 1];
                    sum[2] += data[nb * 3 + 2];
                    cnt += 1;
                }
            }
            if cnt > 0 {
                data[idx * 3] = sum[0] / cnt as f32;
                data[idx * 3 + 1] = sum[1] / cnt as f32;
                data[idx * 3 + 2] = sum[2] / cnt as f32;
                filled[idx] = true;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    // 그래도 남으면(이론상 없음) 항등값
    for idx in 0..n3 {
        if !filled[idx] {
            data[idx * 3] = (idx % size) as f32 / max;
            data[idx * 3 + 1] = ((idx / size) % size) as f32 / max;
            data[idx * 3 + 2] = (idx / (size * size)) as f32 / max;
        }
    }

    // 스무딩 — 격자 노이즈(물·하늘의 얼룩) 제거.
    // 실측 셀은 아주 가볍게(85/15), 추정 셀은 더 세게(50/50).
    // 이로 인해 깎이는 전체 채도·대비는 calibrate_lut()가 목표 통계로 복원한다.
    let src_d = data.clone();
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
                data[idx * 3 + c] = src_d[idx * 3 + c] * keep + sum[c] / cnt as f32 * (1.0 - keep);
            }
        }
    }

    Lut3d { size, data }
}

/// 추출된 LUT 자동 캘리브레이션 — 격자 평균 때문에 뭉개진 대비·채도를
/// 목표(after) 사진의 통계에 맞춰 복원한다.
/// ① src를 LUT로 변환한 결과와 dst의 밝기 평균/표준편차를 맞추고(대비),
/// ② 채도(크로마) 평균 비율만큼 LUT 전체의 크로마를 키운다(진하게).
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
        lut.data[i * 3] = (nl + (r - l) * g_w).clamp(0.0, 1.0);
        lut.data[i * 3 + 1] = (nl + (g - l) * g_w).clamp(0.0, 1.0);
        lut.data[i * 3 + 2] = (nl + (b_ - l) * g_w).clamp(0.0, 1.0);
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
    let use_lut = p.film == "lut" && lut.is_some();

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
            let (lr, lg, lb) = lut.unwrap().sample(r, g, b);
            r = lr;
            g = lg;
            b = lb;
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

    let mut out = vec![0u8; n * 3];
    for i in 0..n * 3 {
        out[i] = (buf[i] * 255.0 + 0.5) as u8;
    }
    out
}
