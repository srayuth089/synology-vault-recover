
// UI for mac-btrfs-synology.
//
// The window shows two panes: Mac disks on the left (destination), the selected
// Synology disk on the right (source). Data flows right-to-left only — there is
// deliberately no control anywhere that writes toward the source.

// Surface any startup failure in the page itself: a blank window is the
// hardest thing to debug, so never fail silently.
window.addEventListener("error", (e) => {
  document.body.insertAdjacentHTML("afterbegin",
    `<pre style="color:#b91c1c;background:#fee2e2;padding:10px;margin:0;white-space:pre-wrap">JS error: ${e.message}\n${e.filename}:${e.lineno}</pre>`);
});

const bridge = window.__TAURI__;
if (!bridge) {
  document.body.insertAdjacentHTML("afterbegin",
    `<pre style="color:#b91c1c;background:#fee2e2;padding:10px;margin:0">window.__TAURI__ missing — withGlobalTauri not active</pre>`);
}
const invoke = bridge?.core?.invoke ?? (async () => { throw new Error("no tauri bridge"); });

const el = (id) => document.getElementById(id);
const state = {
  disks: [], source: null, dest: null,
  // Set once the source disk's three metadata layers have been walked.
  opened: null,      // { label, fsid, bytes_used, subvolumes }
  subvol: null,      // id of the subvolume being browsed
  path: "",          // '/'-separated, relative to the subvolume root
  entries: null,     // listing of `path`, or null while loading
  browseError: null,
  showSystem: false, // DSM's own @-subvolumes are hidden by default
  // Names selected in the current source directory, for a multi-item copy.
  // Cleared whenever the directory or subvolume changes.
  selected: new Set(),
  // Destination side: a real folder, not a disk. Opens in ~/Downloads.
  local: null,       // { dir, entries, free_bytes, can_write }
  copying: null,     // latest CopyProgress while a transfer runs
};

function fmtSize(bytes) {
  if (!bytes) return "—";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0, n = bytes;
  while (n >= 1000 && i < u.length - 1) { n /= 1000; i++; }
  return `${n.toFixed(n >= 100 || i === 0 ? 0 : 1)} ${u[i]}`;
}

const ROLE_LABEL = {
  synology_candidate: "Synology",
  destination_candidate: "ปลายทางได้",
  system_disk: "ระบบ",
  other: "อื่น ๆ",
};

function row({ icon, name, size, meta, selected, dim, onClick, tag, action, picked, onPick }) {
  const div = document.createElement("div");
  div.className = "row" + (selected ? " sel" : "") + (dim ? " dim" : "")
    + (picked ? " picked" : "");
  const nm = document.createElement("div");
  nm.className = "nm";
  // A checkbox is only meaningful where multi-select is offered (onPick set).
  const box = onPick
    ? `<span class="pickbox" title="เลือก">${picked ? "☑" : "☐"}</span>`
    : "";
  nm.innerHTML = `${box}<span class="ico">${icon}</span><span></span>`;
  nm.children[nm.children.length - 1].textContent = name;
  if (onPick) {
    const cb = nm.querySelector(".pickbox");
    cb.addEventListener("click", (ev) => { ev.stopPropagation(); onPick(); });
  }
  if (tag) {
    const t = document.createElement("i");
    t.className = tag.cls;
    t.textContent = tag.text;
    nm.appendChild(t);
  }
  const s = document.createElement("span");
  s.className = "size";
  s.textContent = size;
  const m = document.createElement("span");
  m.className = "meta";
  m.textContent = meta;
  if (action) {
    const b = document.createElement("span");
    b.className = "ejectbtn";
    b.textContent = action.icon;
    b.title = action.title;
    b.addEventListener("click", (ev) => { ev.stopPropagation(); action.run(); });
    m.appendChild(b);
  }
  div.append(nm, s, m);
  if (onClick) div.addEventListener("click", onClick);
  return div;
}

function renderMacPane() {
  const list = el("listMac");
  list.textContent = "";

  if (!state.local) {
    list.innerHTML = `<div class="empty"><span class="spin">◜</span> กำลังเปิดโฟลเดอร์…</div>`;
    return;
  }
  if (!state.local.entries.length) {
    list.innerHTML = `<div class="empty">โฟลเดอร์ว่าง<br>ไฟล์ที่กู้จะมาอยู่ที่นี่</div>`;
    return;
  }
  for (const e of state.local.entries) {
    const isDir = e.kind === "dir";
    list.appendChild(row({
      icon: isDir ? "📁" : "📄",
      name: e.name,
      size: isDir ? "—" : fmtSize(e.size),
      meta: isDir ? "โฟลเดอร์" : "ไฟล์",
      onClick: isDir ? () => openLocal(e.path) : null,
      // /Volumes/<name> entries are mounted disks, so offer to eject them.
      action: e.path.startsWith("/Volumes/") && e.path.split("/").length === 3
        ? { icon: "⏏", title: "ถอดดิสก์", run: () => ejectVolume(e.path) }
        : null,
    }));
  }
}

function fmtWhen(n) { return n ? new Date(n * 1000).toLocaleDateString() : ""; }

/// The right pane: the Synology disk, and once opened, its files.
function renderDiskPick(sources) {
  const dp = el("diskPick");
  // With one disk there is nothing to switch between, so stay out of the way.
  if (sources.length < 2) { dp.hidden = true; return; }
  dp.hidden = false;
  const want = sources.map((d) => d.bsd_name).join(",");
  if (dp.dataset.for !== want) {
    dp.textContent = "";
    for (const d of sources) {
      const o = document.createElement("option");
      o.value = d.bsd_name;
      o.textContent = `${d.media_name || d.bsd_name} · ${fmtSize(d.size_bytes)}`;
      dp.appendChild(o);
    }
    dp.dataset.for = want;
  }
  dp.value = state.source?.bsd_name ?? "";
}

function renderSynoPane() {
  const list = el("listSyno");
  const pick = el("subvolPick");
  list.textContent = "";

  const sources = state.disks.filter((d) => d.role === "synology_candidate");
  renderDiskPick(sources);
  if (!sources.length) {
    pick.hidden = true;
    el("diskPick").hidden = true;
    list.innerHTML = `<div class="empty">ยังไม่พบดิสก์ Synology<br>
      มองหาพาร์ทิชันชนิด <code>Linux_RAID</code><br>
      เสียบดิสก์แล้วกดสแกนใหม่</div>`;
    el("srcPath").textContent = "ดิสก์ Synology — ยังไม่พบ";
    return;
  }

  // Before a disk is opened, the pane still lists candidate disks to pick.
  if (!state.opened) {
    pick.hidden = true;
    for (const d of sources) {
      const chosen = state.source?.bsd_name === d.bsd_name;
      list.appendChild(row({
        icon: "🗄",
        name: d.media_name || d.bsd_name,
        size: fmtSize(d.size_bytes),
        meta: chosen ? "กำลังเปิด…" : "คลิกเพื่อเปิด",
        selected: chosen,
        tag: { cls: "tag ro", text: "RO" },
        onClick: () => selectSource(d),
        action: { icon: "⏏", title: "ถอดดิสก์", run: () => ejectDisk(d.bsd_name) },
      }));
    }
    if (state.browseError) {
      const box = document.createElement("div");
      box.className = "err";
      box.style.whiteSpace = "pre-wrap";
      box.textContent = state.browseError;
      list.prepend(box);
    }
    return;
  }

  // Opened: show the subvolume picker and the current directory.
  pick.hidden = false;
  const subs = state.opened.subvolumes.filter((s) => state.showSystem || !s.system);
  if (pick.dataset.for !== state.opened.fsid || pick.options.length !== subs.length + 1) {
    pick.textContent = "";
    for (const sv of subs) {
      const o = document.createElement("option");
      o.value = String(sv.id);
      o.textContent = sv.name;
      pick.appendChild(o);
    }
    const toggle = document.createElement("option");
    toggle.value = "__toggle__";
    toggle.textContent = state.showSystem ? "— ซ่อนของระบบ —" : "— แสดงของระบบ —";
    pick.appendChild(toggle);
    pick.dataset.for = state.opened.fsid;
  }
  pick.value = String(state.subvol ?? "");

  if (state.browseError) {
    const box = document.createElement("div");
    box.className = "err";
    box.style.whiteSpace = "pre-wrap";
    box.textContent = state.browseError;
    list.appendChild(box);
    return;
  }
  if (state.entries === null) {
    list.innerHTML = `<div class="empty"><span class="spin">◜</span> กำลังอ่าน…</div>`;
    return;
  }
  if (!state.entries.length) {
    list.innerHTML = `<div class="empty">โฟลเดอร์ว่าง</div>`;
    return;
  }

  for (const e of state.entries) {
    const isDir = e.kind === "dir";
    const r = row({
      icon: isDir ? "📁" : e.kind === "symlink" ? "🔗" : "📄",
      name: e.name,
      size: isDir ? "—" : fmtSize(e.size),
      meta: e.subvolume ? "subvol" : isDir ? "โฟลเดอร์" : "ไฟล์",
      dim: e.name.startsWith("@"),
      onClick: isDir ? () => enterDir(e.name) : null,
      picked: state.selected.has(e.name),
      onPick: () => togglePick(e.name),
    });
    // Right-click is the download affordance, as in an FTP client.
    r.addEventListener("contextmenu", (ev) => showMenu(ev, e));
    list.appendChild(r);
  }
}

/// Toggle one entry's checkbox for a multi-item copy.
function togglePick(name) {
  if (state.selected.has(name)) state.selected.delete(name);
  else state.selected.add(name);
  render();
}

/// Breadcrumb trail for the current path, each segment clickable.
function renderCrumbs() {
  const field = el("srcPath");
  if (!state.opened) {
    field.textContent = state.source
      ? `${state.source.bsd_name} — กำลังเปิด…`
      : "ดิสก์ Synology — เลือกต้นทาง";
    el("srcUp").setAttribute("aria-disabled", "true");
    return;
  }
  field.textContent = "";
  const wrap = document.createElement("div");
  wrap.className = "crumbs";

  const parts = state.path.split("/").filter(Boolean);
  const mk = (label, upto) => {
    const a = document.createElement("span");
    a.className = "crumb" + (upto === null ? " now" : "");
    a.textContent = label;
    if (upto !== null) a.addEventListener("click", () => goTo(upto));
    return a;
  };
  wrap.appendChild(mk("/", parts.length ? "" : null));
  parts.forEach((p, i) => {
    const sep = document.createElement("span");
    sep.className = "sep"; sep.textContent = "›";
    wrap.appendChild(sep);
    const last = i === parts.length - 1;
    wrap.appendChild(mk(p, last ? null : parts.slice(0, i + 1).join("/")));
  });
  field.appendChild(wrap);
  el("srcUp").setAttribute("aria-disabled", String(parts.length === 0));
}

function renderLocalPath() {
  const f = el("dstPath");
  if (!state.local) { f.textContent = "กำลังเปิด…"; return; }
  // Show the folder the way Finder does, with ~ for home.
  const home = state.local.dir.match(/^\/Users\/[^/]+/)?.[0];
  const shown = home ? state.local.dir.replace(home, "~") : state.local.dir;
  f.textContent = shown;
  el("dstUp").setAttribute("aria-disabled", String(state.local.dir === "/"));
}

function renderProgress() {
  const box = el("progress");
  const p = state.copying;
  if (!p || (p.finished && !p.errors.length)) { box.hidden = true; return; }
  box.hidden = false;
  const pct = p.bytes_total ? (p.bytes_done / p.bytes_total) * 100 : 0;
  el("pfill").style.width = `${pct.toFixed(1)}%`;
  el("pfill").classList.toggle("done", p.finished);
  el("pnum").textContent = `${Math.round(pct)}%`;
  const ptext = el("ptext");
  ptext.classList.toggle("warn", p.finished && p.errors.length > 0);
  if (p.finished) {
    ptext.textContent = p.errors.length
      ? `เสร็จแล้ว แต่มี ${p.errors.length} ไฟล์ที่อ่านไม่ได้ — ${p.errors[0]}`
      : "เสร็จแล้ว";
  } else {
    ptext.textContent =
      `กำลังคัดลอก — อย่าเพิ่งปิดแอป · ` +
      `${fmtSize(p.bytes_done)} / ${fmtSize(p.bytes_total)} · ` +
      `ไฟล์ ${p.files_done}/${p.files_total} · ${p.current}`;
  }
}

function renderChrome() {
  el("srcName").textContent = state.source
    ? (state.source.media_name || state.source.bsd_name)
    : "ยังไม่ได้เลือก";
  el("destName").textContent = state.local
    ? state.local.dir.split("/").pop() || "/"
    : "กำลังเปิด…";

  renderCrumbs();
  el("btnEject").hidden = !state.source;

  const badge = el("roBadge");
  if (state.opened) {
    badge.textContent = `อ่านอย่างเดียว · ${state.opened.label || state.opened.fsid.slice(0, 8)}`;
    badge.classList.remove("bad");
  } else if (state.source) {
    // macOS cannot set the read-only lock; only the Linux worker can, and only
    // it can prove it. Until then the badge must not imply protection exists.
    badge.textContent = "ต้นทางจะถูกล็อกใน worker ก่อนอ่าน";
    badge.classList.remove("bad");
  } else {
    badge.textContent = "ยังไม่ได้เลือกดิสก์ต้นทาง";
    badge.classList.remove("bad");
  }

  const raid = state.source?.partitions.filter((p) => p.content === "Linux_RAID") ?? [];
  el("stack").textContent = state.opened
    ? `mdadm → lvm → btrfs · ใช้ ${fmtSize(state.opened.bytes_used)}`
    : raid.length
      ? `ชั้นข้อมูล ${raid[0].bsd_name} → md → lvm → btrfs (ยังไม่เปิด)`
      : "ชั้นข้อมูล —";

  el("proof").textContent = state.opened
    ? "✓ เปิดแบบอ่านอย่างเดียว — ไม่มีเส้นทางเขียนในโค้ด"
    : "ยังไม่ได้เปิดดิสก์ต้นทาง";
  el("counts").textContent = state.local
    ? `ปลายทางว่าง ${fmtSize(state.local.free_bytes)}`
    : `พบดิสก์ ${state.disks.length}`;

  // Recovery is possible as soon as a source is open and a folder is shown.
  const ready = Boolean(state.opened && state.local);
  const btn = el("btnRecover");
  btn.setAttribute("aria-disabled", String(!ready));
  btn.textContent = state.selected.size
    ? `← กู้ข้อมูล (${state.selected.size} รายการ)`
    : "← กู้ข้อมูลทั้งหมด";
}

function render() {
  renderMacPane();
  renderSynoPane();
  renderLocalPath();
  renderProgress();
  renderChrome();
}

async function selectSource(d) {
  state.source = d;
  state.opened = null;
  state.browseError = null;
  state.entries = null;
  // Choosing a source can invalidate an already-chosen destination.
  if (state.dest && !(await destAllowed(d, state.dest))) state.dest = null;
  render();
  await openSource(d);
}

/// Walk RAID → LVM → Btrfs and show the disk's subvolumes.
///
/// The data partition is the largest Linux_RAID one; the two small ones are
/// DSM's own system and swap arrays.
async function openSource(d) {
  const raid = d.partitions.filter((p) => p.content === "Linux_RAID");
  const data = raid.slice().sort((a, b) => b.size_bytes - a.size_bytes)[0];
  if (!data) {
    state.browseError = "ดิสก์นี้ไม่มีพาร์ทิชัน Linux_RAID";
    render();
    return;
  }
  // The raw node is far faster for the large sequential reads a tree walk does.
  const device = `/dev/r${data.bsd_name}`;
  try {
    const opened = await invoke("open_source", { device });
    state.opened = opened;
    state.browseError = null;
    const first = opened.subvolumes.find((s) => !s.system) ?? opened.subvolumes[0];
    state.subvol = first ? first.id : null;
    state.path = "";
    render();
    await refreshListing();
  } catch (e) {
    state.opened = null;
    state.browseError = String(e);
    render();
  }
}

async function refreshListing() {
  if (!state.opened || state.subvol === null) return;
  state.entries = null;
  state.browseError = null;
  // A checkbox selection is scoped to the folder it was made in; carrying it
  // into a different folder would let a copy silently include stale entries.
  state.selected.clear();
  render();
  try {
    state.entries = await invoke("list_dir", { subvolId: state.subvol, path: state.path });
  } catch (e) {
    state.entries = [];
    state.browseError = String(e);
  }
  render();
}

function enterDir(name) {
  state.path = state.path ? `${state.path}/${name}` : name;
  refreshListing();
}

function goTo(path) {
  state.path = path;
  refreshListing();
}

function goUp() {
  if (!state.path) return;
  const parts = state.path.split("/").filter(Boolean);
  parts.pop();
  goTo(parts.join("/"));
}

async function selectDest(d) {
  if (state.source && !(await destAllowed(state.source, d))) return;
  state.dest = d;
  render();
}

async function destAllowed(src, dest) {
  try {
    await invoke("check_destination", { sourceBsd: src.bsd_name, destBsd: dest.bsd_name });
    return true;
  } catch (e) {
    alert(String(e));
    return false;
  }
}

async function scan() {
  const overview = await invoke("scan_disks");
  state.disks = overview.disks ?? [];
  // Drop selections whose disk went away between scans.
  const present = (d) => d && state.disks.some((x) => x.bsd_name === d.bsd_name);
  if (!present(state.source)) {
    state.source = null;
    state.opened = null; // the disk it described is gone
    state.entries = null;
  }
  if (!present(state.dest)) state.dest = null;

  render();
  if (overview.error) {
    const box = document.createElement("div");
    box.className = "err";
    box.textContent = `สแกนดิสก์ไม่สำเร็จ: ${overview.error}`;
    el("listMac").prepend(box);
  }
}

/// Open a local folder in the destination pane.
async function openLocal(dir) {
  try {
    state.local = await invoke("list_local", { dir: dir ?? null });
  } catch (e) {
    alert(String(e));
  }
  render();
}

/// Eject a mounted volume shown in the destination pane.
async function ejectVolume(mountPath) {
  try {
    await invoke("eject", { bsdName: mountPath });
    await openLocal(state.local.dir);
  } catch (e) {
    alert(String(e));
  }
}

function localUp() {
  if (!state.local || state.local.dir === "/") return;
  const parts = state.local.dir.split("/").filter(Boolean);
  parts.pop();
  openLocal("/" + parts.join("/"));
}

/// Ask for a destination folder using the system picker.
async function pickLocal() {
  try {
    const chosen = await window.__TAURI__.dialog.open({
      directory: true,
      defaultPath: state.local?.dir,
      title: "เลือกโฟลเดอร์ปลายทาง",
    });
    if (chosen) await openLocal(chosen);
  } catch {
    // The dialog plugin may be unavailable; the pane is still navigable.
  }
}

// ---- right-click menu on the source side ----

function hideMenu() { el("ctx").hidden = true; }

/// Show the download menu for one source entry, FileZilla-style.
///
/// When items are checked and the right-clicked row is one of them, the menu
/// acts on the whole checked set — right-clicking any checked row is enough,
/// there is no need to re-click each one first.
function showMenu(ev, entry) {
  ev.preventDefault();
  const menu = el("ctx");
  menu.textContent = "";

  const names = state.selected.size && state.selected.has(entry.name)
    ? [...state.selected]
    : [entry.name];

  const dest = state.local?.dir;
  const add = (label, icon, fn, enabled = true) => {
    const b = document.createElement("button");
    b.innerHTML = `<span>${icon}</span>`;
    b.appendChild(document.createTextNode(label));
    b.disabled = !enabled;
    if (enabled) b.addEventListener("click", () => { hideMenu(); fn(); });
    menu.appendChild(b);
  };

  const where = dest ? dest.split("/").pop() : "—";
  const label = names.length > 1 ? `${names.length} รายการที่เลือก` : "";
  add(`ดาวน์โหลด${label ? ` ${label}` : ""}ไปที่ ${where}`, "⬇", () => download(names), Boolean(dest));
  add("ดาวน์โหลดไปที่…", "📂", () => downloadTo(names), true);

  const sep = document.createElement("div");
  sep.className = "sepline";
  menu.appendChild(sep);
  const info = names.length > 1
    ? `${names.length} รายการ`
    : `ขนาด ${entry.kind === "dir" ? "— (โฟลเดอร์)" : fmtSize(entry.size)}`;
  add(info, "ℹ", () => {}, false);

  menu.hidden = false;
  // Keep the menu on screen near the pointer.
  const r = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(ev.clientX, innerWidth - r.width - 8)}px`;
  menu.style.top = `${Math.min(ev.clientY, innerHeight - r.height - 8)}px`;
}

async function downloadTo(names) {
  await pickLocal();
  if (state.local) download(names);
}

/// Copy one or more entries from the current source folder into the
/// destination folder, asking about conflicts only when there are any.
async function download(names) {
  if (!state.opened || !state.local) return;
  const paths = names.map((n) => (state.path ? `${state.path}/${n}` : n));
  const dest = state.local.dir;

  let plan;
  try {
    plan = await invoke("plan_copy", { subvolId: state.subvol, paths, dest });
  } catch (e) {
    alert(String(e));
    return;
  }

  const policy = plan.conflicts.length ? await askConflictPolicy(plan) : "overwrite";
  if (!policy) return; // user cancelled

  try {
    await invoke("copy_out", { subvolId: state.subvol, paths, dest, policy });
    state.selected.clear();
    pollCopy();
  } catch (e) {
    alert(String(e));
  }
}

/// Ask the user how to handle files that already exist at the destination.
///
/// Returns the chosen `ConflictPolicy` string, or null if the user cancelled
/// the whole copy rather than picking one of the three choices.
async function askConflictPolicy(plan) {
  const n = plan.conflicts.length;
  const sample = plan.conflicts.slice(0, 3).map((c) => `  • ${c.rel_path}`).join("\n");
  const more = n > 3 ? `\n  …และอีก ${n - 3} ไฟล์` : "";
  const msg =
    `พบไฟล์ที่มีอยู่แล้วที่ปลายทาง ${n} ไฟล์:\n${sample}${more}\n\n` +
    `ต้องการทำอย่างไร?`;
  return showConflictDialog(msg);
}

/// A small modal with the three overwrite choices, since a plain confirm()
/// only offers two. Resolves to a ConflictPolicy string, or null on cancel.
function showConflictDialog(message) {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `
      <div class="modal">
        <pre class="modal-msg"></pre>
        <div class="modal-actions">
          <button data-v="overwrite_if_different">ทับเฉพาะที่ไม่ตรงกัน (แนะนำ)</button>
          <button data-v="skip">ไม่ทับ — ข้ามไฟล์ที่มีอยู่แล้ว</button>
          <button data-v="overwrite">ทับทั้งหมด</button>
          <button data-v="" class="cancel">ยกเลิก</button>
        </div>
      </div>`;
    overlay.querySelector(".modal-msg").textContent = message;
    overlay.addEventListener("click", (ev) => {
      const btn = ev.target.closest("button");
      if (!btn && ev.target !== overlay) return;
      const v = btn ? btn.dataset.v : "";
      overlay.remove();
      resolve(v || null);
    });
    document.body.appendChild(overlay);
  });
}

/// Poll copy progress until it finishes, refreshing the destination pane.
///
/// A single failed poll (e.g. the backend is briefly busy flushing a large
/// write) must not silently stop the loop — that is exactly what made a
/// long-running copy look like the app had frozen or died.
function pollCopy() {
  window.__mbsCopyActive = true;
  const tick = async () => {
    if (!window.__mbsCopyActive) return;
    try {
      state.copying = await invoke("copy_progress");
      renderProgress();
    } catch {
      // Keep retrying; a transient IPC hiccup is not the copy finishing.
      setTimeout(tick, 250);
      return;
    }
    if (!state.copying.finished) {
      setTimeout(tick, 200);
      return;
    }
    window.__mbsCopyActive = false;
    // Show what landed, then clear the bar unless something failed.
    await openLocal(state.local.dir);
    if (!state.copying.errors.length) {
      setTimeout(() => { state.copying = null; renderProgress(); }, 2500);
    }
  };
  tick();
}

/// Detach a disk safely. Closing our read handle is part of the same step,
/// because macOS will not unmount a disk something still holds open.
async function ejectDisk(bsdName) {
  const wasOpen = state.source?.bsd_name === bsdName;
  try {
    await invoke("eject", { bsdName });
  } catch (e) {
    alert(String(e));
    return;
  }
  if (wasOpen) {
    state.source = null;
    state.opened = null;
    state.entries = null;
    state.subvol = null;
    state.path = "";
  }
  await scan();
}

el("btnRescan").addEventListener("click", scan);
el("btnEject").addEventListener("click", () => {
  if (state.source) ejectDisk(state.source.bsd_name);
});
el("dstUp").addEventListener("click", localUp);
el("dstPick").addEventListener("click", pickLocal);
document.addEventListener("click", hideMenu);
document.addEventListener("contextmenu", (e) => {
  // Suppress the WebView's own menu everywhere except our rows.
  if (!e.target.closest("#listSyno .row")) { e.preventDefault(); hideMenu(); }
});
el("srcUp").addEventListener("click", goUp);
el("diskPick").addEventListener("change", (ev) => {
  const d = state.disks.find((x) => x.bsd_name === ev.target.value);
  if (d && d.bsd_name !== state.source?.bsd_name) selectSource(d);
});
el("subvolPick").addEventListener("change", (ev) => {
  const v = ev.target.value;
  if (v === "__toggle__") {
    // Reveal or hide DSM's own subvolumes without losing the current one.
    state.showSystem = !state.showSystem;
    el("subvolPick").dataset.for = "";
    render();
    return;
  }
  state.subvol = Number(v);
  state.path = "";
  refreshListing();
});
el("btnRecover").addEventListener("click", () => {
  if (el("btnRecover").getAttribute("aria-disabled") === "true") return;
  // Checked items take priority: the user picked specific things. Otherwise
  // recover everything visible in the folder shown right now.
  const names = state.selected.size
    ? [...state.selected]
    : (state.entries ?? []).map((e) => e.name);
  if (names.length) download(names);
});

// Warn before the window closes mid-copy: a heavy write burst can make the UI
// feel briefly unresponsive, and closing then would leave a partial file.
window.addEventListener("beforeunload", (ev) => {
  if (state.copying && !state.copying.finished) {
    ev.preventDefault();
    ev.returnValue = "";
  }
});

scan();
openLocal();

