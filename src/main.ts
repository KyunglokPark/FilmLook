import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog, save as saveDialog, message } from "@tauri-apps/plugin-dialog";

// ---- 필름 시뮬레이션 목록 (process.rs의 recipe()와 id가 일치해야 함) ----
const FILMS = [
  { id: "provia", name: "Provia", desc: "표준" },
  { id: "velvia", name: "Velvia", desc: "선명·고채도" },
  { id: "astia", name: "Astia", desc: "부드러움" },
  { id: "classic_chrome", name: "Classic Chrome", desc: "다큐 감성" },
  { id: "classic_neg", name: "Classic Neg", desc: "네거 필름" },
  { id: "eterna", name: "Eterna", desc: "시네마" },
  { id: "vintage_warm", name: "Vintage Warm", desc: "바랜 따뜻한 필름" },
  { id: "acros", name: "Acros", desc: "흑백" },
];

let lutName: string | null = null; // 로드된 3D LUT (HaldCLUT/.cube) 이름

interface Params {
  film: string;
  exposure: number;
  contrast: number;
  saturation: number;
  temperature: number;
  sharpen: number;
  grain: number;
  vignette: number;
}

const DEFAULTS: Params = {
  film: "provia",
  exposure: 0,
  contrast: 0,
  saturation: 0,
  temperature: 0,
  sharpen: 0,
  grain: 0,
  vignette: 0,
};

// 슬라이더 정의: [키, 라벨, 최소, 최대, 스텝]
const SLIDERS: [keyof Params, string, number, number, number][] = [
  ["exposure", "노출", -1, 1, 0.02],
  ["contrast", "대비", -1, 1, 0.02],
  ["saturation", "채도", -1, 1, 0.02],
  ["temperature", "색온도", -1, 1, 0.02],
  ["sharpen", "샤픈", 0, 1, 0.02],
  ["grain", "그레인", 0, 1, 0.02],
  ["vignette", "비네트", 0, 1, 0.02],
];

let params: Params = { ...DEFAULTS };
let filePath: string | null = null;
let originalPreview = ""; // 보정 전 미리보기 (원본 보기용)
let lastPreview = ""; // 마지막 보정 미리보기 (1:1 모드에서 복귀용)
let imgW = 0; // 원본 픽셀 크기
let imgH = 0;

const previewEl = document.getElementById("preview") as HTMLImageElement;
const previewOrigEl = document.getElementById("preview-orig") as HTMLImageElement;
const compareWrapEl = document.getElementById("compare-wrap") as HTMLDivElement;
const compareDividerEl = document.getElementById("compare-divider") as HTMLDivElement;
const placeholderEl = document.getElementById("placeholder") as HTMLDivElement;
const busyEl = document.getElementById("busy") as HTMLDivElement;
const fileInfoEl = document.getElementById("file-info") as HTMLSpanElement;
const statusEl = document.getElementById("status") as HTMLSpanElement;
const openBtn = document.getElementById("act-open") as HTMLButtonElement;
const saveBtn = document.getElementById("act-save") as HTMLButtonElement;
const compareBtn = document.getElementById("act-compare") as HTMLButtonElement;
const resetBtn = document.getElementById("act-reset") as HTMLButtonElement;
const extractBtn = document.getElementById("act-extract") as HTMLButtonElement;
const exportLutBtn = document.getElementById("act-export-lut") as HTMLButtonElement;
const filmsEl = document.getElementById("films") as HTMLDivElement;
const slidersEl = document.getElementById("sliders") as HTMLDivElement;

function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

// ---- 미리보기 렌더 (디바운스 + 동시 실행 1개) ----
let renderTimer: number | null = null;
let rendering = false;
let renderAgain = false;

function scheduleRender(delay?: number) {
  if (!filePath) return;
  // 1:1(원본 해상도) 모드는 렌더 비용이 커서 디바운스를 길게
  const d = delay ?? (mode11 ? 350 : 90);
  if (renderTimer !== null) clearTimeout(renderTimer);
  renderTimer = window.setTimeout(() => void doRender(), d);
}

async function doRender() {
  if (rendering) {
    renderAgain = true;
    return;
  }
  rendering = true;
  busyEl.classList.remove("hidden");
  try {
    const cmd = mode11 ? "render_full" : "render_preview";
    const url = await invoke<string>(cmd, { params });
    if (!mode11) lastPreview = url;
    previewEl.src = url;
  } catch (e) {
    statusEl.textContent = String(e);
  } finally {
    rendering = false;
    busyEl.classList.add("hidden");
    if (renderAgain) {
      renderAgain = false;
      void doRender();
    }
  }
}

// ---- UI 구성 ----
function renderFilms() {
  filmsEl.innerHTML = "";
  for (const f of FILMS) {
    const card = document.createElement("div");
    card.className = "film" + (params.film === f.id ? " active" : "");
    const n = document.createElement("div");
    n.className = "fname";
    n.textContent = f.name;
    const d = document.createElement("div");
    d.className = "fdesc";
    d.textContent = f.desc;
    card.append(n, d);
    card.addEventListener("click", () => {
      params.film = f.id;
      renderFilms();
      scheduleRender(0);
    });
    filmsEl.appendChild(card);
  }

  // 사용자 3D LUT — 실측 필름 LUT(HaldCLUT PNG / .cube)를 그대로 적용
  const lutCard = document.createElement("div");
  lutCard.className = "film" + (params.film === "lut" ? " active" : "");
  const ln = document.createElement("div");
  ln.className = "fname";
  ln.textContent = lutName ? `LUT: ${lutName}` : "LUT 불러오기…";
  const ld = document.createElement("div");
  ld.className = "fdesc";
  ld.textContent = lutName ? "클릭해 적용 · 더블클릭해 교체" : "HaldCLUT PNG / .cube";
  lutCard.append(ln, ld);
  lutCard.addEventListener("click", () => {
    if (lutName) {
      params.film = "lut";
      renderFilms();
      scheduleRender(0);
    } else {
      void pickLut();
    }
  });
  lutCard.addEventListener("dblclick", () => void pickLut());
  filmsEl.appendChild(lutCard);
}

/// 보정 전/후 사진 두 개를 골라 색 변환(LUT)을 추출한다
async function extractRecipe() {
  const before = await openDialog({
    multiple: false,
    directory: false,
    title: "① 보정 전(원본) 사진 선택",
    filters: [{ name: "이미지", extensions: ["jpg", "jpeg", "png", "bmp", "webp"] }],
  });
  if (!before || Array.isArray(before)) return;
  const after = await openDialog({
    multiple: false,
    directory: false,
    title: "② 원하는 색감(보정 후) 사진 선택",
    filters: [{ name: "이미지", extensions: ["jpg", "jpeg", "png", "bmp", "webp"] }],
  });
  if (!after || Array.isArray(after)) return;
  busyEl.classList.remove("hidden");
  statusEl.textContent = "색감 추출 중…";
  try {
    const info = await invoke<string>("extract_lut", { before, after });
    lutName = `추출 레시피 ${info}`;
    params.film = "lut";
    exportLutBtn.disabled = false;
    renderFilms();
    scheduleRender(0);
    statusEl.textContent = "레시피 추출 완료 — 지금 보이는 게 적용된 결과야";
  } catch (e) {
    statusEl.textContent = String(e);
    await message(String(e), { title: "iDFilm", kind: "error" });
  } finally {
    busyEl.classList.add("hidden");
  }
}

async function exportLut() {
  const picked = await saveDialog({
    defaultPath: "recipe.cube",
    filters: [{ name: "3D LUT", extensions: ["cube"] }],
  });
  if (!picked) return;
  try {
    await invoke("export_lut", { path: picked });
    statusEl.textContent = `LUT 저장됨: ${baseName(picked)}`;
  } catch (e) {
    await message(String(e), { title: "iDFilm", kind: "error" });
  }
}

async function pickLut() {
  const picked = await openDialog({
    multiple: false,
    directory: false,
    filters: [{ name: "3D LUT", extensions: ["png", "cube"] }],
  });
  if (!picked || Array.isArray(picked)) return;
  busyEl.classList.remove("hidden");
  try {
    const info = await invoke<string>("load_lut", { path: picked });
    lutName = `${baseName(picked)} (${info})`;
    params.film = "lut";
    exportLutBtn.disabled = false;
    renderFilms();
    scheduleRender(0);
  } catch (e) {
    await message(String(e), { title: "iDFilm", kind: "error" });
  } finally {
    busyEl.classList.add("hidden");
  }
}

const valueEls = new Map<string, HTMLSpanElement>();
const rangeEls = new Map<string, HTMLInputElement>();

function fmt(v: number): string {
  return (v > 0 ? "+" : "") + v.toFixed(2).replace(/\.?0+$/, "") || "0";
}

function renderSliders() {
  slidersEl.innerHTML = "";
  for (const [key, label, min, max, step] of SLIDERS) {
    const row = document.createElement("div");
    row.className = "slider-row";
    const head = document.createElement("div");
    head.className = "slider-head";
    const name = document.createElement("span");
    name.textContent = label;
    const val = document.createElement("span");
    val.className = "val";
    val.textContent = fmt(params[key] as number);
    head.append(name, val);
    const range = document.createElement("input");
    range.type = "range";
    range.min = String(min);
    range.max = String(max);
    range.step = String(step);
    range.value = String(params[key]);
    range.addEventListener("input", () => {
      (params[key] as number) = parseFloat(range.value);
      val.textContent = fmt(parseFloat(range.value));
      scheduleRender();
    });
    // 더블클릭으로 해당 값만 초기화
    head.addEventListener("dblclick", () => {
      (params[key] as number) = DEFAULTS[key] as number;
      range.value = String(DEFAULTS[key]);
      val.textContent = fmt(DEFAULTS[key] as number);
      scheduleRender(0);
    });
    valueEls.set(key, val);
    rangeEls.set(key, range);
    row.append(head, range);
    slidersEl.appendChild(row);
  }
}

function syncSliders() {
  for (const [key] of SLIDERS) {
    const r = rangeEls.get(key);
    const v = valueEls.get(key);
    if (r) r.value = String(params[key]);
    if (v) v.textContent = fmt(params[key] as number);
  }
}

// ---- 동작 ----
async function openImage() {
  const picked = await openDialog({
    multiple: false,
    directory: false,
    filters: [{ name: "이미지", extensions: ["jpg", "jpeg", "png", "bmp", "webp"] }],
  });
  if (!picked || Array.isArray(picked)) return;
  busyEl.classList.remove("hidden");
  statusEl.textContent = "이미지 여는 중…";
  try {
    const result = await invoke<{ width: number; height: number; preview: string }>("open_image", {
      path: picked,
    });
    filePath = picked;
    originalPreview = result.preview;
    lastPreview = result.preview;
    imgW = result.width;
    imgH = result.height;
    origFullUrl = null;
    mode11 = false;
    resetZoom();
    previewEl.src = result.preview;
    previewOrigEl.src = result.preview;
    compareWrapEl.classList.remove("hidden");
    placeholderEl.style.display = "none";
    saveBtn.disabled = false;
    compareBtn.disabled = false;
    updateZoomInfo();
    statusEl.textContent = `${baseName(picked)} — 휠로 확대, / 로 1:1 보기`;
    scheduleRender(0);
  } catch (e) {
    statusEl.textContent = String(e);
    await message(String(e), { title: "iDFilm", kind: "error" });
  } finally {
    busyEl.classList.add("hidden");
  }
}

async function saveImage() {
  if (!filePath) return;
  const base = baseName(filePath);
  const dot = base.lastIndexOf(".");
  const suggested = (dot > 0 ? base.slice(0, dot) : base) + "_idfilm.jpg";
  const picked = await saveDialog({
    defaultPath: suggested,
    filters: [
      { name: "JPEG", extensions: ["jpg", "jpeg"] },
      { name: "PNG", extensions: ["png"] },
    ],
  });
  if (!picked) return;
  busyEl.classList.remove("hidden");
  statusEl.textContent = "저장 중… (원본 해상도 처리)";
  try {
    await invoke("save_image", { path: picked, params });
    statusEl.textContent = `저장됨: ${baseName(picked)}`;
  } catch (e) {
    statusEl.textContent = String(e);
    await message(String(e), { title: "iDFilm", kind: "error" });
  } finally {
    busyEl.classList.add("hidden");
  }
}

function resetAll() {
  params = { ...DEFAULTS };
  renderFilms();
  syncSliders();
  scheduleRender(0);
}

// ---- 확대/축소 · 패닝 · 1:1(원본 해상도) 보기 ----
let zoom = 1; // 1 = 화면 맞춤
let tx = 0;
let ty = 0;
let mode11 = false; // 원본 해상도 렌더 모드 (/ 토글)
let origFullUrl: string | null = null; // 보정 전 원본 전체 해상도 (1:1 비교용 캐시)

function fitScale(): number {
  if (!imgW || !imgH) return 1;
  const r = compareWrapEl.parentElement!.getBoundingClientRect();
  return Math.min(r.width / imgW, r.height / imgH);
}

function applyTransform() {
  compareWrapEl.style.transform = `translate(${tx}px, ${ty}px) scale(${zoom})`;
  compareWrapEl.style.cursor = zoom > 1.001 ? "grab" : "";
  updateZoomInfo();
}

function resetZoom() {
  zoom = 1;
  tx = 0;
  ty = 0;
  applyTransform();
}

function clampPan() {
  const r = compareWrapEl.parentElement!.getBoundingClientRect();
  const maxX = (r.width * zoom) / 2;
  const maxY = (r.height * zoom) / 2;
  tx = Math.max(-maxX, Math.min(maxX, tx));
  ty = Math.max(-maxY, Math.min(maxY, ty));
}

function updateZoomInfo() {
  if (!imgW) return;
  const pct = Math.round(fitScale() * zoom * 100);
  fileInfoEl.textContent = `${filePath ? baseName(filePath) : ""} · ${imgW}×${imgH} · ${pct}%${mode11 ? " (1:1)" : ""}`;
}

/// "/" — 원본 해상도(100%) 보기 토글. 미리보기(1400px) 확대는 흐릿하므로
/// 이 모드에선 원본 해상도로 실제 렌더해서 디테일을 검수할 수 있다.
async function toggle11() {
  if (!filePath) return;
  if (!mode11) {
    mode11 = true;
    busyEl.classList.remove("hidden");
    statusEl.textContent = "원본 해상도 렌더 중…";
    try {
      const url = await invoke<string>("render_full", { params });
      previewEl.src = url;
      if (comparing) {
        if (!origFullUrl) origFullUrl = await invoke<string>("original_full");
        previewOrigEl.src = origFullUrl;
      }
      zoom = 1 / fitScale(); // 화면에서 1 이미지픽셀 = 1 화면픽셀
      tx = 0;
      ty = 0;
      applyTransform();
      statusEl.textContent = "1:1 보기 — 드래그로 이동, / 로 복귀";
    } catch (e) {
      mode11 = false;
      statusEl.textContent = String(e);
    } finally {
      busyEl.classList.add("hidden");
    }
  } else {
    mode11 = false;
    previewEl.src = lastPreview;
    previewOrigEl.src = originalPreview;
    resetZoom();
    statusEl.textContent = "";
  }
}

// 휠 줌 (커서 위치 기준)
const viewerEl = document.getElementById("viewer") as HTMLDivElement;
viewerEl.addEventListener(
  "wheel",
  (e) => {
    if (!filePath) return;
    e.preventDefault();
    const r = compareWrapEl.parentElement!.getBoundingClientRect();
    const cx = r.left + r.width / 2;
    const cy = r.top + r.height / 2;
    const factor = Math.pow(1.0015, -e.deltaY);
    const maxZoom = Math.max(8, (1 / fitScale()) * 3);
    const nz = Math.min(maxZoom, Math.max(1, zoom * factor));
    // 커서 아래 지점이 고정되도록 이동량 보정
    tx = e.clientX - cx - ((e.clientX - cx - tx) * nz) / zoom;
    ty = e.clientY - cy - ((e.clientY - cy - ty) * nz) / zoom;
    zoom = nz;
    if (zoom <= 1.001) {
      tx = 0;
      ty = 0;
    }
    clampPan();
    applyTransform();
  },
  { passive: false },
);

// 드래그 패닝 (확대 상태에서) / 클릭은 비교 디바이더 이동
let panning = false;
let panStart = { x: 0, y: 0, tx: 0, ty: 0 };
let panMoved = false;

// ---- 원본/보정 비교 슬라이더 (가운데 핸들 드래그) ----
let comparing = false;

async function setCompare(on: boolean) {
  if (!originalPreview) return;
  comparing = on;
  compareWrapEl.classList.toggle("comparing", on);
  if (on) {
    compareWrapEl.style.setProperty("--cmp", "50%");
    if (mode11) {
      // 1:1 모드에서 비교 켜면 원본도 전체 해상도로
      if (!origFullUrl) {
        try {
          origFullUrl = await invoke<string>("original_full");
        } catch {
          return;
        }
      }
      previewOrigEl.src = origFullUrl;
    }
  }
}

function moveDivider(clientX: number) {
  const rect = compareWrapEl.getBoundingClientRect();
  const pct = Math.min(100, Math.max(0, ((clientX - rect.left) / rect.width) * 100));
  compareWrapEl.style.setProperty("--cmp", `${pct}%`);
}

let draggingDivider = false;
compareDividerEl.addEventListener("pointerdown", (e) => {
  draggingDivider = true;
  compareDividerEl.setPointerCapture(e.pointerId);
});
compareDividerEl.addEventListener("pointermove", (e) => {
  if (draggingDivider) moveDivider(e.clientX);
});
compareDividerEl.addEventListener("pointerup", () => {
  draggingDivider = false;
});
// 확대 상태에선 드래그로 패닝, 클릭(이동 없음)이면 비교 디바이더 이동
compareWrapEl.addEventListener("pointerdown", (e) => {
  if (e.target === compareDividerEl || compareDividerEl.contains(e.target as Node)) return;
  panning = zoom > 1.001;
  panMoved = false;
  panStart = { x: e.clientX, y: e.clientY, tx, ty };
  if (panning) {
    compareWrapEl.setPointerCapture(e.pointerId);
    compareWrapEl.style.cursor = "grabbing";
  }
});
compareWrapEl.addEventListener("pointermove", (e) => {
  if (!panning) return;
  const dx = e.clientX - panStart.x;
  const dy = e.clientY - panStart.y;
  if (Math.abs(dx) + Math.abs(dy) > 4) panMoved = true;
  tx = panStart.tx + dx;
  ty = panStart.ty + dy;
  clampPan();
  applyTransform();
});
compareWrapEl.addEventListener("pointerup", (e) => {
  const wasPan = panning && panMoved;
  panning = false;
  compareWrapEl.style.cursor = zoom > 1.001 ? "grab" : "";
  if (!wasPan && comparing && e.target !== compareDividerEl) moveDivider(e.clientX);
});
// 더블클릭 = 1:1 토글
compareWrapEl.addEventListener("dblclick", () => void toggle11());

openBtn.addEventListener("click", () => void openImage());
saveBtn.addEventListener("click", () => void saveImage());
resetBtn.addEventListener("click", resetAll);
compareBtn.addEventListener("click", () => void setCompare(!comparing));
extractBtn.addEventListener("click", () => void extractRecipe());
exportLutBtn.addEventListener("click", () => void exportLut());

window.addEventListener("keydown", (e) => {
  const mod = e.ctrlKey || e.metaKey;
  if (mod && e.key.toLowerCase() === "o") {
    e.preventDefault();
    void openImage();
  } else if (mod && e.key.toLowerCase() === "s") {
    e.preventDefault();
    void saveImage();
  } else if (e.key === "\\") {
    // \ 키로도 비교 토글
    void setCompare(!comparing);
  } else if (e.key === "/") {
    // / 키 — 원본 크기(1:1) 보기 토글
    e.preventDefault();
    void toggle11();
  }
});

renderFilms();
renderSliders();
statusEl.textContent = "이미지를 열어 필름 시뮬레이션을 적용해보세요";
