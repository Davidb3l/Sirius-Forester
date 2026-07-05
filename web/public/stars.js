/* Sirius Forester Console — living cosmos.
   A real Webb nebula photograph (public-domain NASA/ESA/CSA "Cosmic Cliffs",
   shipped locally at /nebula.jpg — no external request) drifting slowly under a
   twinkling starfield, occasional shooting stars, and a fine film grain that
   ties the layers together. A dark scrim keeps the UI legible.

   Zero deps. Pauses when hidden. Honors prefers-reduced-motion (static, no
   drift). #fx-toggle drops to a plain static background for low-power devices. */
(() => {
  const canvas = document.getElementById("starfield");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const btn = document.getElementById("fx-toggle");
  const reduceMedia = matchMedia("(prefers-reduced-motion: reduce)");
  const TILE = 512;

  let mode = localStorage.getItem("sirius-fx");
  if (mode !== "full" && mode !== "plain") mode = reduceMedia.matches ? "plain" : "full";

  // ---- the nebula photograph ----
  const neb = new Image();
  let nebReady = false;
  neb.onload = () => { nebReady = true; };
  neb.src = "/nebula.jpg";

  // ---- fine grain tile (unifies photo + drawn stars) ----
  function grainSVG() {
    return (
      `<svg xmlns='http://www.w3.org/2000/svg' width='${TILE}' height='${TILE}'>` +
      `<filter id='g'><feTurbulence type='fractalNoise' baseFrequency='0.8' numOctaves='2' stitchTiles='stitch' result='n'/>` +
      `<feColorMatrix in='n' type='matrix' values='0.8 0 0 0 0.1 0.8 0 0 0 0.1 0.8 0 0 0 0.13 0 0 0 0 0.5'/></filter>` +
      `<rect width='100%' height='100%' filter='url(#g)'/></svg>`
    );
  }
  let grainTile = null;
  (() => {
    const img = new Image();
    img.onload = () => {
      const c = document.createElement("canvas");
      c.width = TILE; c.height = TILE;
      c.getContext("2d").drawImage(img, 0, 0, TILE, TILE);
      grainTile = c;
    };
    img.src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(grainSVG());
  })();

  // ---- stars (twinkle + drift over the photo) ----
  const COLORS = ["#cfe8ff", "#dbeeff", "#ffffff", "#b8d2ff", "#ffe1b8", "#e9f3ff"];
  const WEIGHT = [0.3, 0.22, 0.15, 0.15, 0.08, 0.1];
  let W = 0, H = 0, DPR = 1, stars = [];
  let shooting = [], nextShoot = 2500, raf = 0, last = 0;

  function pickColor() {
    let r = Math.random(), a = 0;
    for (let i = 0; i < COLORS.length; i++) { a += WEIGHT[i]; if (r <= a) return COLORS[i]; }
    return COLORS[0];
  }
  function build() {
    DPR = Math.min(window.devicePixelRatio || 1, 2);
    W = canvas.clientWidth || window.innerWidth;
    H = canvas.clientHeight || window.innerHeight;
    canvas.width = Math.round(W * DPR);
    canvas.height = Math.round(H * DPR);
    ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
    const count = Math.min(460, Math.round((W * H) / 7200));
    stars = [];
    for (let i = 0; i < count; i++) {
      const b = Math.random();
      const r = b > 0.96 ? 1.3 + Math.random() * 1.0 : b > 0.75 ? 0.8 + Math.random() * 0.6 : 0.35 + Math.random() * 0.5;
      stars.push({
        x: Math.random() * W, y: Math.random() * H, r,
        base: 0.3 + Math.random() * 0.5, amp: 0.16 + Math.random() * 0.44,
        spd: 0.0004 + Math.random() * 0.0016, ph: Math.random() * Math.PI * 2,
        col: pickColor(),
        vx: (Math.random() - 0.5) * 0.0035 * r,
        vy: (0.0014 + Math.random() * 0.0026) * (0.4 + r * 0.3),
        halo: b > 0.92,
      });
    }
  }

  // Ken-Burns: cover the canvas at a little extra scale, then slowly pan + zoom
  // within that margin so the photo never shows an edge.
  function paintNebula(t) {
    if (!nebReady) return;
    const iw = neb.naturalWidth, ih = neb.naturalHeight;
    const zoom = 1.14 + 0.04 * Math.sin(t * 0.00003);
    const scale = Math.max(W / iw, H / ih) * zoom;
    const dw = iw * scale, dh = ih * scale;
    const mx = dw - W, my = dh - H;
    const px = 0.5 + 0.42 * Math.sin(t * 0.000021);
    const py = 0.5 + 0.42 * Math.cos(t * 0.000017);
    ctx.drawImage(neb, -mx * px, -my * py, dw, dh);
    // scrim: darker at the top (header + content) and bottom (footer), so the
    // photo reads as atmosphere, not a busy foreground.
    const g = ctx.createLinearGradient(0, 0, 0, H);
    g.addColorStop(0, "rgba(4,6,13,0.86)");
    g.addColorStop(0.34, "rgba(4,6,13,0.46)");
    g.addColorStop(0.72, "rgba(4,6,13,0.4)");
    g.addColorStop(1, "rgba(4,6,13,0.6)");
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, W, H);
  }
  function paintGrain() {
    if (!grainTile) return;
    ctx.globalCompositeOperation = "soft-light";
    ctx.globalAlpha = 0.35;
    const ox = Math.random() * TILE, oy = Math.random() * TILE;
    const sx = -(((ox % TILE) + TILE) % TILE), sy = -(((oy % TILE) + TILE) % TILE);
    for (let y = sy; y < H; y += TILE)
      for (let x = sx; x < W; x += TILE) ctx.drawImage(grainTile, x, y);
    ctx.globalCompositeOperation = "source-over";
    ctx.globalAlpha = 1;
  }

  function paintStar(s, alpha) {
    ctx.fillStyle = s.col;
    if (s.halo) { ctx.globalAlpha = alpha * 0.16; ctx.beginPath(); ctx.arc(s.x, s.y, s.r * 2.8, 0, 7); ctx.fill(); }
    ctx.globalAlpha = Math.min(1, alpha);
    ctx.beginPath(); ctx.arc(s.x, s.y, s.r, 0, 7); ctx.fill();
  }
  function spawnShoot() {
    const fromLeft = Math.random() < 0.5;
    const speed = 0.45 + Math.random() * 0.35;
    shooting.push({ x: fromLeft ? -40 : W + 40, y: Math.random() * H * 0.42, vx: fromLeft ? speed : -speed, vy: 0.14 + Math.random() * 0.16, len: 110 + Math.random() * 90, life: 0, max: (W + 240) / speed });
  }
  function drawShoot(sh, dt) {
    sh.x += sh.vx * dt; sh.y += sh.vy * dt; sh.life += dt;
    const mag = Math.hypot(sh.vx, sh.vy) || 1;
    const tx = sh.x - (sh.vx / mag) * sh.len, ty = sh.y - (sh.vy / mag) * sh.len;
    const a = Math.sin(Math.min(1, sh.life / sh.max) * Math.PI);
    const g = ctx.createLinearGradient(sh.x, sh.y, tx, ty);
    g.addColorStop(0, `rgba(214,236,255,${0.85 * a})`);
    g.addColorStop(1, "rgba(214,236,255,0)");
    ctx.strokeStyle = g; ctx.lineWidth = 1.6; ctx.lineCap = "round";
    ctx.beginPath(); ctx.moveTo(sh.x, sh.y); ctx.lineTo(tx, ty); ctx.stroke();
    ctx.globalAlpha = a; ctx.fillStyle = "#eaf5ff"; ctx.beginPath(); ctx.arc(sh.x, sh.y, 1.5, 0, 7); ctx.fill(); ctx.globalAlpha = 1;
  }

  function frame(t) {
    const dt = Math.min(50, t - (last || t));
    last = t;
    ctx.clearRect(0, 0, W, H);
    paintNebula(t);
    paintGrain();
    ctx.globalCompositeOperation = "lighter";
    for (const s of stars) {
      s.x += s.vx * dt; s.y += s.vy * dt;
      if (s.x < -3) s.x = W + 3; else if (s.x > W + 3) s.x = -3;
      if (s.y > H + 3) { s.y = -3; s.x = Math.random() * W; }
      const a = s.base + s.amp * Math.sin(s.ph + t * s.spd);
      if (a > 0.02) paintStar(s, a);
    }
    if (t > nextShoot) { spawnShoot(); nextShoot = t + 7000 + Math.random() * 10000; }
    for (let i = shooting.length - 1; i >= 0; i--) { drawShoot(shooting[i], dt); if (shooting[i].life > shooting[i].max) shooting.splice(i, 1); }
    ctx.globalCompositeOperation = "source-over";
    ctx.globalAlpha = 1;
    raf = requestAnimationFrame(frame);
  }

  function drawStaticOnce() {
    let tries = 0;
    const compose = () => {
      ctx.clearRect(0, 0, W, H);
      paintNebula(2000);
      paintGrain();
      ctx.globalCompositeOperation = "lighter";
      for (const s of stars) paintStar(s, s.base + s.amp * 0.4);
      ctx.globalCompositeOperation = "source-over";
      ctx.globalAlpha = 1;
      if (!nebReady && tries++ < 60) setTimeout(compose, 120);
    };
    compose();
  }

  function apply() {
    document.body.classList.toggle("fx-plain", mode !== "full");
    if (btn) { btn.setAttribute("aria-pressed", String(mode === "full")); btn.textContent = mode === "full" ? "✦" : "○"; }
    cancelAnimationFrame(raf);
    if (mode === "full" && !reduceMedia.matches) { last = 0; raf = requestAnimationFrame(frame); }
    else { ctx.clearRect(0, 0, W, H); if (mode === "full") drawStaticOnce(); }
  }

  if (btn) btn.addEventListener("click", () => {
    mode = mode === "full" ? "plain" : "full";
    try { localStorage.setItem("sirius-fx", mode); } catch (e) {}
    apply();
  });

  let resizeTimer = 0;
  window.addEventListener("resize", () => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => { build(); apply(); }, 180);
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) cancelAnimationFrame(raf);
    else if (mode === "full" && !reduceMedia.matches) { last = 0; raf = requestAnimationFrame(frame); }
  });

  build();
  apply();
})();
