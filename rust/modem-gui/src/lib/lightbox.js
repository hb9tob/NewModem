// Shared widget — fullscreen image lightbox (wheel/drag/keyboard zoom+pan).
// Opened from the RX received-file panel and the History tab. Self-contained:
// owns its `lightbox` state, touches only its own DOM, and reaches the OS window
// for true fullscreen via the ipc wrapper.
import { getCurrentWindow } from "./ipc.js";

export const LIGHTBOX_MAX_SCALE = 8;

export const lightbox = {
  viewEl: null,
  imgEl: null,
  natW: 0,
  natH: 0,
  minScale: 1,
  maxScale: LIGHTBOX_MAX_SCALE,
  scale: 1,
  tx: 0,
  ty: 0,
  dragging: false,
  lastX: 0,
  lastY: 0,
  wasFullscreen: false,
};

export async function setWindowFullscreen(flag) {
  try {
    const win = getCurrentWindow();
    await win.setFullscreen(flag);
  } catch (err) {
    console.error("setFullscreen", err);
  }
}

export function waitForResize(prevW, prevH, timeoutMs = 400) {
  return new Promise((resolve) => {
    if (window.innerWidth !== prevW || window.innerHeight !== prevH) {
      return resolve();
    }
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      window.removeEventListener("resize", finish);
      resolve();
    };
    window.addEventListener("resize", finish);
    setTimeout(finish, timeoutMs);
  });
}

export async function openLightbox(src, alt) {
  lightbox.viewEl = document.getElementById("image-lightbox");
  lightbox.imgEl = document.getElementById("image-lightbox-img");
  if (!lightbox.viewEl || !lightbox.imgEl) return;
  lightbox.imgEl.alt = alt || "";
  lightbox.imgEl.onload = () => {
    lightbox.natW = lightbox.imgEl.naturalWidth || 1;
    lightbox.natH = lightbox.imgEl.naturalHeight || 1;
    fitLightbox();
  };
  lightbox.imgEl.src = src;
  lightbox.viewEl.hidden = false;
  // OS fullscreen via Tauri: the browser requestFullscreen only fullscreens
  // the WebView inside the window, not the window itself.
  try {
    const win = getCurrentWindow();
    lightbox.wasFullscreen = await win.isFullscreen();
    if (!lightbox.wasFullscreen) {
      const prevW = window.innerWidth;
      const prevH = window.innerHeight;
      await win.setFullscreen(true);
      // Wait for the resize to propagate WebView-side before fitting,
      // otherwise we compute the center with the windowed dimensions and
      // the image appears offset toward the top-left corner.
      await waitForResize(prevW, prevH);
    }
  } catch (err) {
    console.error("isFullscreen/setFullscreen", err);
  }
  // If the image is cached, onload may not refire - explicit refit with the
  // final viewport size.
  if (lightbox.imgEl.complete && lightbox.imgEl.naturalWidth > 0) {
    lightbox.natW = lightbox.imgEl.naturalWidth;
    lightbox.natH = lightbox.imgEl.naturalHeight;
  }
  fitLightbox();
}

export async function closeLightbox() {
  if (!lightbox.viewEl) return;
  lightbox.viewEl.hidden = true;
  lightbox.imgEl.src = "";
  if (!lightbox.wasFullscreen) {
    await setWindowFullscreen(false);
  }
}

export function clampLightboxPan() {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const w = lightbox.natW * lightbox.scale;
  const h = lightbox.natH * lightbox.scale;
  if (w <= vw) {
    lightbox.tx = (vw - w) / 2;
  } else {
    lightbox.tx = Math.max(vw - w, Math.min(0, lightbox.tx));
  }
  if (h <= vh) {
    lightbox.ty = (vh - h) / 2;
  } else {
    lightbox.ty = Math.max(vh - h, Math.min(0, lightbox.ty));
  }
}

export function applyLightboxTransform() {
  if (!lightbox.imgEl) return;
  clampLightboxPan();
  const { imgEl, scale, tx, ty } = lightbox;
  imgEl.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
}

export function fitLightbox() {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  // fit = what makes the whole image fit in the viewport, capped at 1:1
  // (no auto-upscale for small images).
  const fit = Math.min(vw / lightbox.natW, vh / lightbox.natH, 1);
  lightbox.minScale = fit;
  lightbox.maxScale = LIGHTBOX_MAX_SCALE;
  lightbox.scale = fit;
  lightbox.tx = (vw - lightbox.natW * fit) / 2;
  lightbox.ty = (vh - lightbox.natH * fit) / 2;
  applyLightboxTransform();
}

export function zoomLightboxBy(factor, cx, cy) {
  const prev = lightbox.scale;
  let next = prev * factor;
  next = Math.max(lightbox.minScale, Math.min(lightbox.maxScale, next));
  if (next === prev) return;
  // Zoom centered on (cx, cy): this point in screen coords stays fixed.
  lightbox.tx = cx - (cx - lightbox.tx) * (next / prev);
  lightbox.ty = cy - (cy - lightbox.ty) * (next / prev);
  lightbox.scale = next;
  applyLightboxTransform();
}

export function zoomLightbox(delta, cx, cy) {
  zoomLightboxBy(Math.exp(-delta * 0.0015), cx, cy);
}

export function panLightbox(dx, dy) {
  lightbox.tx += dx;
  lightbox.ty += dy;
  applyLightboxTransform();
}

export function setupLightbox() {
  const view = document.getElementById("image-lightbox");
  if (!view) return;
  view.addEventListener("wheel", (ev) => {
    if (view.hidden) return;
    ev.preventDefault();
    zoomLightbox(ev.deltaY, ev.clientX, ev.clientY);
  }, { passive: false });
  view.addEventListener("mousedown", (ev) => {
    if (view.hidden) return;
    lightbox.dragging = true;
    lightbox.lastX = ev.clientX;
    lightbox.lastY = ev.clientY;
    view.classList.add("dragging");
  });
  window.addEventListener("mousemove", (ev) => {
    if (!lightbox.dragging) return;
    lightbox.tx += ev.clientX - lightbox.lastX;
    lightbox.ty += ev.clientY - lightbox.lastY;
    lightbox.lastX = ev.clientX;
    lightbox.lastY = ev.clientY;
    applyLightboxTransform();
  });
  window.addEventListener("mouseup", () => {
    if (!lightbox.dragging) return;
    lightbox.dragging = false;
    view.classList.remove("dragging");
  });
  // Single click on the background (not the image) closes. Double-click also closes.
  view.addEventListener("click", (ev) => {
    if (ev.target === view) closeLightbox();
  });
  view.addEventListener("dblclick", closeLightbox);
  window.addEventListener("keydown", (ev) => {
    if (view.hidden) return;
    const cx = window.innerWidth / 2;
    const cy = window.innerHeight / 2;
    const PAN_STEP = 60;
    // We look at both key AND code: on some layouts (Swiss AZERTY), the
    // numpad does not surface "+"/"-" as key, but NumpadAdd/Subtract is
    // always there.
    const isPlus = ev.key === "+" || ev.key === "=" || ev.key === "a" || ev.key === "A" || ev.code === "NumpadAdd";
    const isMinus = ev.key === "-" || ev.key === "_" || ev.key === "q" || ev.key === "Q" || ev.code === "NumpadSubtract";
    const isZero = ev.key === "0" || ev.code === "Numpad0";
    if (ev.key === "Escape") {
      closeLightbox();
    } else if (isPlus) {
      zoomLightboxBy(1.25, cx, cy);
      ev.preventDefault();
    } else if (isMinus) {
      zoomLightboxBy(1 / 1.25, cx, cy);
      ev.preventDefault();
    } else if (isZero) {
      fitLightbox();
      ev.preventDefault();
    } else if (ev.key === "ArrowLeft") {
      panLightbox(PAN_STEP, 0);
      ev.preventDefault();
    } else if (ev.key === "ArrowRight") {
      panLightbox(-PAN_STEP, 0);
      ev.preventDefault();
    } else if (ev.key === "ArrowUp") {
      panLightbox(0, PAN_STEP);
      ev.preventDefault();
    } else if (ev.key === "ArrowDown") {
      panLightbox(0, -PAN_STEP);
      ev.preventDefault();
    }
  });
  // Resize fires after Tauri setFullscreen has resized the window, which
  // re-centers the image in the new viewport.
  window.addEventListener("resize", () => {
    if (!view.hidden) fitLightbox();
  });
}
