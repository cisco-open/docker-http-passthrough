// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.

const prefersReducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

const state = {
  mode: "docker",
  passthroughHost: "10.20.30.40:8080",
  registryScheme: "https",
  registryHost: "docker.io",
  imagePath: "nginx",
  imageTag: "latest",
  flashName: "nginx.tar",
};

const hints = {
  docker: "Default mode uses the registry configured on the passthrough service.",
  registry: "Registry URL mode puts the target registry in the request path and can use anonymous, static, or optional client credentials.",
  "registry-auth": "Registry + Auth mode challenges the client and forwards its Basic credentials upstream for this request only.",
};

function qs(selector, root = document) {
  return root.querySelector(selector);
}

function qsa(selector, root = document) {
  return Array.from(root.querySelectorAll(selector));
}

function encodeImageFilename(imagePath, tag) {
  const normalizedPath = imagePath
    .trim()
    .replace(/^\/+|\/+$/g, "")
    .replace(/\/+/g, "/");
  const encodedPath = normalizedPath.split("/").join("__");
  const safeTag = tag.trim() || "latest";
  return `${encodedPath}_${safeTag}.tar`;
}

function buildUrl() {
  const host = state.passthroughHost.trim() || "localhost:8080";
  const file = encodeImageFilename(state.imagePath, state.imageTag);

  if (state.mode === "docker") {
    return `http://${host}/docker/${file}`;
  }

  const registryHost = state.registryHost.trim() || "docker.io";
  return `http://${host}/${state.mode}/${state.registryScheme}/${registryHost}/docker/${file}`;
}

function renderBuilder() {
  const url = buildUrl();
  const cli = `copy ${url} flash:${state.flashName.trim() || "image.tar"}`;
  const parsedImage = `${state.imagePath.trim().replace(/^\/+|\/+$/g, "")}:${state.imageTag.trim() || "latest"}`;

  qs("#output-url").textContent = url;
  qs("#output-cli").textContent = cli;
  qs("#parsed-image").textContent = parsedImage;
  qs("#builder-hint").textContent = hints[state.mode];

  const registryControlsDisabled = state.mode === "docker";
  qs("#registry-scheme").disabled = registryControlsDisabled;
  qs("#registry-host").disabled = registryControlsDisabled;
}

function bindBuilder() {
  qsa('input[name="mode"]').forEach((input) => {
    input.addEventListener("change", () => {
      state.mode = input.value;
      renderBuilder();
    });
  });

  const bindings = [
    ["#passthrough-host", "passthroughHost"],
    ["#registry-scheme", "registryScheme"],
    ["#registry-host", "registryHost"],
    ["#image-path", "imagePath"],
    ["#image-tag", "imageTag"],
    ["#flash-name", "flashName"],
  ];

  bindings.forEach(([selector, key]) => {
    const field = qs(selector);
    field.addEventListener("input", () => {
      state[key] = field.value;
      renderBuilder();
    });
    field.addEventListener("change", () => {
      state[key] = field.value;
      renderBuilder();
    });
  });

  renderBuilder();
}

async function copyText(targetId) {
  const target = document.getElementById(targetId);
  if (!target) return;
  const text = target.innerText || target.textContent || "";

  try {
    await navigator.clipboard.writeText(text.trim());
    showToast("Copied");
  } catch {
    const area = document.createElement("textarea");
    area.value = text.trim();
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    document.execCommand("copy");
    area.remove();
    showToast("Copied");
  }
}

let toastTimer;
function showToast(message) {
  const toast = qs(".toast");
  toast.textContent = message;
  toast.classList.add("visible");
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => toast.classList.remove("visible"), 1500);
}

function bindCopyButtons() {
  qsa("[data-copy-target]").forEach((button) => {
    button.addEventListener("click", () => copyText(button.dataset.copyTarget));
  });
}

function bindTabs() {
  qsa("[data-tabs]").forEach((tabs) => {
    const buttons = qsa("[data-tab]", tabs);
    const panels = qsa("[data-panel]", tabs);

    buttons.forEach((button) => {
      button.addEventListener("click", () => {
        const tab = button.dataset.tab;

        buttons.forEach((item) => {
          item.setAttribute("aria-selected", item === button ? "true" : "false");
        });
        panels.forEach((panel) => {
          panel.classList.toggle("active", panel.dataset.panel === tab);
        });
      });
    });
  });
}

function bindTopology() {
  const stage = qs("[data-topology-stage]");
  const buttons = qsa("[data-topology]");
  if (!stage || !buttons.length) return;

  buttons.forEach((button) => {
    button.addEventListener("click", () => {
      const mode = button.dataset.topology;
      stage.classList.toggle("auth", mode === "auth");
      stage.dataset.mode = mode;

      buttons.forEach((item) => {
        item.setAttribute("aria-selected", item === button ? "true" : "false");
      });
    });
  });
}

function bindTraceToggle() {
  const toggle = qs("[data-trace-toggle]");
  const tree = qs(".trace-tree");
  if (!toggle || !tree) return;

  toggle.addEventListener("click", () => {
    tree.classList.toggle("collapsed");
    toggle.classList.toggle("expanded", !tree.classList.contains("collapsed"));
  });
}

function bindNav() {
  const nav = qs("[data-nav]");
  const menu = qs("#nav-menu");
  const toggle = qs(".nav-toggle");
  const links = qsa(".nav-links a[href^='#']");

  function updateNavState() {
    nav.classList.toggle("scrolled", window.scrollY > 18);

    const current = links
      .map((link) => {
        const section = qs(link.getAttribute("href"));
        if (!section) return null;
        return { link, top: section.getBoundingClientRect().top };
      })
      .filter(Boolean)
      .filter((item) => item.top < 140)
      .pop();

    links.forEach((link) => link.classList.toggle("active", current?.link === link));
  }

  toggle.addEventListener("click", () => {
    const open = !menu.classList.contains("open");
    menu.classList.toggle("open", open);
    toggle.setAttribute("aria-expanded", String(open));
  });

  links.forEach((link) => {
    link.addEventListener("click", () => {
      menu.classList.remove("open");
      toggle.setAttribute("aria-expanded", "false");
    });
  });

  window.addEventListener("scroll", updateNavState, { passive: true });
  updateNavState();
}

function bindReveals() {
  const items = qsa(".reveal");

  if (!("IntersectionObserver" in window)) {
    items.forEach((item) => item.classList.add("visible"));
    return;
  }

  const observer = new IntersectionObserver(
    (entries) => {
      entries.forEach((entry) => {
        if (entry.isIntersecting) {
          entry.target.classList.add("visible");
          observer.unobserve(entry.target);
        }
      });
    },
    { threshold: 0.14 }
  );

  items.forEach((item) => observer.observe(item));
}

function animateFlow() {
  const nodes = qsa(".flow-node");
  if (!nodes.length || prefersReducedMotion) return;

  let index = 0;
  window.setInterval(() => {
    nodes.forEach((node, nodeIndex) => node.classList.toggle("active", nodeIndex === index));
    index = (index + 1) % nodes.length;
  }, 1700);
}

function startHeroCanvas() {
  const canvas = qs("#hero-canvas");
  if (!canvas) return;

  const context = canvas.getContext("2d");
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const packets = [];
  const paths = [
    { from: [0.12, 0.34], to: [0.48, 0.28], color: "#25a9f5" },
    { from: [0.48, 0.28], to: [0.78, 0.42], color: "#36d399" },
    { from: [0.2, 0.68], to: [0.5, 0.58], color: "#ffcc66" },
    { from: [0.5, 0.58], to: [0.83, 0.68], color: "#25a9f5" },
    { from: [0.14, 0.5], to: [0.82, 0.5], color: "#8ef6c4" },
  ];

  paths.forEach((path, pathIndex) => {
    for (let i = 0; i < 4; i += 1) {
      packets.push({
        pathIndex,
        progress: i / 4,
        speed: 0.0012 + pathIndex * 0.00018,
      });
    }
  });

  function resize() {
    const rect = canvas.getBoundingClientRect();
    canvas.width = Math.floor(rect.width * dpr);
    canvas.height = Math.floor(rect.height * dpr);
    context.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  function point(pair) {
    return [pair[0] * canvas.clientWidth, pair[1] * canvas.clientHeight];
  }

  function drawNode(x, y, label, color) {
    context.save();
    context.strokeStyle = color;
    context.fillStyle = "rgba(7, 16, 24, 0.72)";
    context.lineWidth = 1;
    context.beginPath();
    context.roundRect(x - 46, y - 22, 92, 44, 7);
    context.fill();
    context.stroke();
    context.fillStyle = "rgba(238, 247, 255, 0.84)";
    context.font = "700 12px system-ui, sans-serif";
    context.textAlign = "center";
    context.textBaseline = "middle";
    context.fillText(label, x, y);
    context.restore();
  }

  function draw() {
    context.clearRect(0, 0, canvas.clientWidth, canvas.clientHeight);

    context.save();
    context.globalAlpha = 0.32;
    context.strokeStyle = "rgba(139, 181, 211, 0.32)";
    context.lineWidth = 1;
    const grid = 58;
    for (let x = 0; x < canvas.clientWidth; x += grid) {
      context.beginPath();
      context.moveTo(x, 0);
      context.lineTo(x, canvas.clientHeight);
      context.stroke();
    }
    for (let y = 0; y < canvas.clientHeight; y += grid) {
      context.beginPath();
      context.moveTo(0, y);
      context.lineTo(canvas.clientWidth, y);
      context.stroke();
    }
    context.restore();

    paths.forEach((path) => {
      const [x1, y1] = point(path.from);
      const [x2, y2] = point(path.to);
      context.strokeStyle = "rgba(139, 181, 211, 0.25)";
      context.lineWidth = 2;
      context.beginPath();
      context.moveTo(x1, y1);
      context.lineTo(x2, y2);
      context.stroke();
    });

    drawNode(...point([0.12, 0.34]), "Cisco CLI", "#25a9f5");
    drawNode(...point([0.48, 0.28]), "Passthrough", "#36d399");
    drawNode(...point([0.78, 0.42]), "Registry", "#ffcc66");
    drawNode(...point([0.83, 0.68]), "Tar Stream", "#8ef6c4");

    if (!prefersReducedMotion) {
      packets.forEach((packet) => {
        const path = paths[packet.pathIndex];
        packet.progress = (packet.progress + packet.speed) % 1;
        const [x1, y1] = point(path.from);
        const [x2, y2] = point(path.to);
        const x = x1 + (x2 - x1) * packet.progress;
        const y = y1 + (y2 - y1) * packet.progress;

        context.fillStyle = path.color;
        context.beginPath();
        context.roundRect(x - 6, y - 3, 12, 6, 3);
        context.fill();
      });
    }

    window.requestAnimationFrame(draw);
  }

  resize();
  window.addEventListener("resize", resize);
  draw();
}

function init() {
  bindBuilder();
  bindCopyButtons();
  bindTabs();
  bindTopology();
  bindTraceToggle();
  bindNav();
  bindReveals();
  animateFlow();
  startHeroCanvas();
}

init();
