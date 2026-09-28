// MULTI web GUI: settings, status and live captions. Plain JS, no build step.
"use strict";

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const MASK = "********";

function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === false || v == null) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "text") el.textContent = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

async function api(method, path, body) {
  const opts = { method, headers: { "X-Multi": "1" } };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const r = await fetch(path, opts);
  if (r.status === 401) { location.href = "/login"; throw new Error("sign in required"); }
  let data = null;
  try { data = await r.json(); } catch (_) { /* empty body */ }
  return { ok: r.ok, status: r.status, data };
}

// ------------------------------------------------------------ paths

function parsePath(path) {
  const keys = [];
  for (const part of path.split(".")) {
    const m = part.match(/^([^[]+)((?:\[\d+\])*)$/);
    if (!m) { keys.push(part); continue; }
    keys.push(m[1]);
    for (const i of m[2].matchAll(/\[(\d+)\]/g)) keys.push(Number(i[1]));
  }
  return keys;
}
function getPath(obj, path) {
  return parsePath(path).reduce((o, k) => (o == null ? undefined : o[k]), obj);
}
function setPath(obj, path, value) {
  const keys = parsePath(path);
  let o = obj;
  for (const k of keys.slice(0, -1)) o = o[k];
  o[keys[keys.length - 1]] = value;
}
const clone = (x) => JSON.parse(JSON.stringify(x));

// ------------------------------------------------------------ settings model

const LANG_NAMES = {
  en: "English", es: "Spanish", fr: "French", de: "German", it: "Italian", pt: "Portuguese",
  nl: "Dutch", pl: "Polish", ru: "Russian", uk: "Ukrainian", ja: "Japanese", ko: "Korean",
  zh: "Chinese", ar: "Arabic", hi: "Hindi", sv: "Swedish", da: "Danish", fi: "Finnish", no: "Norwegian",
};

// Help text from PRD §5 (docs/prd/05-configuration-and-tuning.md).
const SECTIONS = [
  { id: "input", title: "Input", lede: "Where the program feed comes from. H.264 or HEVC in MPEG-TS; video is never re-encoded.", custom: renderInput },
  { id: "outputs", title: "Outputs", lede: "Where the captioned stream goes. Each output runs and restarts on its own.", custom: renderOutputs },
  { id: "languages", title: "Languages", lede: "One spoken (source) language; every other language is a translation. Each language needs a CEA-608 channel, a CEA-708 service, or both.", custom: renderLanguages },
  {
    id: "captions", title: "Captions", lede: "How captions look on screen.",
    fields: [
      { path: "captions.mode", label: "Mode", type: "select", options: [["roll-up", "Roll-up"], ["pop-on", "Pop-on"], ["paint-on", "Paint-on"]], help: "Roll-up is fast and live; pop-on reads cleaner." },
      { path: "captions.rows", label: "Rows", type: "number", min: 1, max: 4, help: "1–4. Screen coverage vs context." },
      { path: "captions.max_chars_per_line", label: "Characters per line", type: "number", min: 20, max: 42, help: "20–42 (CEA-608 allows at most 32). Readability vs line breaks." },
      { path: "captions.clear_after_ms", label: "Clear after", unit: "ms", type: "number", min: 1000, max: 30000, step: 500, help: "1,000–30,000. How long text lingers after speech stops." },
      { path: "captions.offset_ms", label: "Caption offset", unit: "ms", type: "number", min: -5000, max: 5000, step: 50, help: "−5,000 to +5,000. Fine-tune caption timing against video." },
    ],
  },
  {
    id: "filter", title: "Word filter", lede: "Applied to every caption language before it is sent.",
    fields: [
      { path: "filter.profanity", label: "Mask explicit language", type: "checkbox", help: "Built-in list, in every language." },
      { path: "filter.mask_style", label: "Mask style", type: "select", options: [["first-letter", "First letter (f***)"], ["asterisks", "Asterisks (****)"], ["bleep", "[bleep]"], ["drop", "Drop the word"]] },
      { path: "filter.blocklist", label: "Always mask", type: "lines", wide: true, help: "One word per line, any caption language. * matches any letters (e.g. darn*)." },
      { path: "filter.allowlist", label: "Never mask", type: "lines", wide: true, help: "One word per line. Wins over every list." },
    ],
  },
  {
    id: "timing", title: "Speech recognition and translation", lede: "Latency against accuracy.",
    fields: [
      { path: "asr.model", label: "ASR model", type: "text", help: "Accuracy vs speed and VRAM." },
      { path: "asr.chunk_ms", label: "ASR chunk", unit: "ms", type: "number", min: 160, max: 3000, help: "Nemotron exports: 160, 560 or 1,120. Lower lag vs accuracy." },
      { path: "asr.stability_passes", label: "Stability passes", type: "number", min: 1, max: 3, help: "1–3. Lower lag vs fewer on-screen corrections." },
      { path: "vad.threshold", label: "Voice detection threshold", type: "number", min: 0.1, max: 0.9, step: 0.05, float: true, help: "0.1–0.9. Catching quiet speech vs ignoring noise and music." },
      { path: "translate.segment", label: "Translate by", type: "select", options: [["word", "Word"], ["clause", "Clause"], ["sentence", "Sentence"]], help: "Translation lag vs quality." },
      { path: "translate.max_wait_ms", label: "Max wait for a clause", unit: "ms", type: "number", min: 200, max: 3000, step: 50, help: "200–3,000. Upper limit on waiting for a clause to finish." },
      { path: "degrade.max_lag_ms", label: "Shed load above", unit: "ms lag", type: "number", min: 1000, max: 30000, step: 500, help: "1,000–30,000. When to start shedding load to protect the stream." },
    ],
  },
  {
    id: "video", title: "Video, audio and SRT", lede: "",
    fields: [
      { path: "video.delay_ms", label: "Video delay", unit: "ms", type: "number", min: 0, max: 10000, step: 100, help: "0 = pass-through. Hold video back so captions line up with speech; compare with the caption lag on the Status tab." },
      { path: "audio.track", label: "Audio track", type: "number", min: 0, help: "0 = first track. Which mic feed is transcribed." },
      { path: "audio.channel", label: "Audio channel", type: "optnum", min: 0, placeholder: "All (downmix)", help: "Leave blank to downmix all channels." },
      { path: "srt.latency_ms", label: "SRT latency", unit: "ms", type: "number", min: 20, max: 8000, step: 10, help: "20–8,000. Network resilience vs delay. A URL's own latency= wins." },
    ],
  },
  {
    id: "gpu", title: "GPU", lede: "",
    fields: [
      { path: "gpu.device", label: "CUDA device", type: "optnum", min: 0, placeholder: "Auto", help: "Blank picks automatically and falls back to CPU." },
      { path: "gpu.max_vram_mb", label: "VRAM limit", unit: "MB", type: "optnum", min: 0, placeholder: "No limit", help: "For sharing a GPU with an encoder." },
    ],
  },
  {
    id: "web", title: "Web GUI", lede: "Address changes apply when multi serve restarts.",
    fields: [
      { path: "web.bind", label: "Listen address", type: "text", mono: true, help: "127.0.0.1 = this machine only; 0.0.0.0 = all networks (needs a password or token; HTTPS turns on by itself)." },
      { path: "web.port", label: "Port", type: "number", min: 1, max: 65535, help: "Also serves the REST API." },
      { path: "web.token", label: "API token", type: "password", optional: true, help: "For scripts: Authorization: Bearer <token>. People sign in with the password (multi passwd). MULTI_WEB_TOKEN wins over this." },
      { path: "web.autostart", label: "Start the pipeline when multi serve starts", type: "checkbox" },
    ],
  },
];

const S = { saved: null, draft: null, issues: [], status: null };

function dirty() { return JSON.stringify(S.saved) !== JSON.stringify(S.draft); }

function onChange() {
  const d = dirty();
  $("#save").disabled = !d;
  $("#revert").disabled = !d;
  const note = $("#save-note");
  note.textContent = d ? "Unsaved changes" : "No changes";
  note.classList.toggle("dirty", d);
  updateSavebar();
}

function errSlot(path) { return h("p", { class: "err-msg", "data-err": path, id: "err-" + path }); }

function bindInput(el, path, kind) {
  const read = () => {
    const v = el.type === "checkbox" ? el.checked : el.value;
    switch (kind) {
      case "number": return v === "" ? 0 : Number(v);
      case "optnum": return v === "" ? null : Number(v);
      case "lines": return v.split("\n").map((s) => s.trim()).filter(Boolean);
      case "optional": return v === "" ? null : v;
      default: return v;
    }
  };
  el.dataset.path = path;
  el.setAttribute("aria-describedby", "err-" + path);
  el.addEventListener(el.type === "checkbox" || el.tagName === "SELECT" ? "change" : "input", () => {
    setPath(S.draft, path, read());
    onChange();
  });
}

function renderField(f) {
  const id = "f-" + f.path;
  const v = getPath(S.draft, f.path);
  let input;
  if (f.type === "checkbox") {
    input = h("input", { type: "checkbox", id });
    input.checked = !!v;
    bindInput(input, f.path, "bool");
    return h("div", { class: "field" + (f.wide ? " wide" : "") },
      h("label", { class: "check", for: id }, input, f.label),
      f.help && h("p", { class: "help" }, f.help), errSlot(f.path));
  }
  if (f.type === "select") {
    input = h("select", { id }, f.options.map(([val, text]) => h("option", { value: val, selected: val === v }, text)));
    bindInput(input, f.path, "text");
  } else if (f.type === "lines") {
    input = h("textarea", { id, rows: 5, spellcheck: "false" });
    input.value = (v || []).join("\n");
    bindInput(input, f.path, "lines");
  } else if (f.type === "number" || f.type === "optnum") {
    input = h("input", { type: "number", id, min: f.min, max: f.max, step: f.step || (f.float ? "any" : 1), placeholder: f.placeholder, inputmode: f.float ? "decimal" : "numeric" });
    input.value = v == null ? "" : v;
    bindInput(input, f.path, f.type);
  } else {
    input = h("input", { type: f.type === "password" ? "password" : "text", id, class: f.mono ? "mono" : null, autocomplete: "off", spellcheck: "false" });
    input.value = v == null ? "" : v;
    bindInput(input, f.path, f.optional ? "optional" : "text");
  }
  return h("div", { class: "field" + (f.wide ? " wide" : "") },
    h("label", { for: id }, f.label, f.unit && h("span", { class: "unit" }, " (" + f.unit + ")")),
    input, f.help && h("p", { class: "help" }, f.help), errSlot(f.path));
}

// ------------------------------------------------------------ endpoints

function splitUrl(url) {
  const m = (url || "").match(/^([a-z]+):\/\/([^?]*)(?:\?(.*))?$/i);
  if (!m) return { scheme: "", rest: url || "", params: [] };
  const params = (m[3] || "").split("&").filter(Boolean).map((kv) => {
    const i = kv.indexOf("=");
    return i < 0 ? [kv, ""] : [kv.slice(0, i), kv.slice(i + 1)];
  });
  return { scheme: m[1].toLowerCase(), rest: m[2], params };
}
function hostPort(rest) {
  const m = rest.match(/^(\[[^\]]*\]|[^:/]*)(?::(\d+))?/);
  return { host: m ? m[1] : "", port: m && m[2] ? m[2] : "" };
}
function joinQuery(params) {
  const q = params.filter(([, v]) => v !== "" && v != null).map(([k, v]) => k + "=" + v).join("&");
  return q ? "?" + q : "";
}

// Helper fields for one URL. `kinds`: allowed schemes. Calls set(url) on change.
function endpointEditor(path, kinds, isOutput) {
  const url = getPath(S.draft, path);
  const box = h("div", { class: "fields" });
  const urlId = "f-" + path;
  const urlInput = h("input", { type: "text", id: urlId, class: "mono", spellcheck: "false", autocomplete: "off" });
  urlInput.value = url;
  urlInput.dataset.path = path;
  urlInput.setAttribute("aria-describedby", "err-" + path);

  const commit = (u) => { setPath(S.draft, path, u); urlInput.value = u; onChange(); };

  function draw() {
    box.replaceChildren();
    const cur = getPath(S.draft, path);
    const p = splitUrl(cur);
    const scheme = kinds.includes(p.scheme) ? p.scheme : kinds[0];
    const typeSel = h("select", { "aria-label": "Protocol" },
      kinds.map((k) => h("option", { value: k, selected: k === scheme }, k.toUpperCase())));
    typeSel.addEventListener("change", () => {
      const s = typeSel.value;
      const def = { srt: "srt://0.0.0.0:9001?mode=listener", udp: "udp://239.0.0.1:5000", rtp: "rtp://0.0.0.0:5004", rtmp: "rtmp://a.rtmp.youtube.com/live2/", rtmps: "rtmps://" };
      commit(def[s] || s + "://");
      draw();
    });
    const g = h("div", { class: "grid" }, h("div", { class: "field" }, h("span", { class: "label" }, "Protocol"), typeSel));
    const mk = (label, value, attrs, onin, help) => {
      const id = "h-" + path + "-" + label.replace(/\W/g, "");
      const i = h("input", Object.assign({ id, type: "text", autocomplete: "off", spellcheck: "false" }, attrs));
      i.value = value;
      i.addEventListener("input", () => commit(onin(i)));
      g.append(h("div", { class: "field" }, h("label", { for: id }, label), i, help && h("p", { class: "help" }, help)));
      return i;
    };
    if (scheme === "rtmp" || scheme === "rtmps") {
      const segs = p.rest.split("/");
      const hasKey = segs.length > 2;
      const server = scheme + "://" + (hasKey ? segs.slice(0, -1) : segs).join("/");
      const key = hasKey ? segs[segs.length - 1] : "";
      const masked = key === "***";
      let srv = server, k = key;
      const build = () => srv.replace(/\/+$/, "") + "/" + k;
      mk("Server URL", server, { class: "mono" }, (i) => { srv = i.value; return build(); }, "e.g. rtmp://a.rtmp.youtube.com/live2");
      mk("Stream key", masked ? "" : key, { type: "password", placeholder: masked ? "Saved — leave blank to keep" : "" },
        (i) => { k = i.value === "" && masked ? "***" : i.value; return build(); }, "Never shown after saving.");
    } else {
      const { host, port } = hostPort(p.rest);
      const params = p.params.slice();
      const get = (k) => (params.find(([n]) => n === k) || [])[1] || "";
      const put = (k, v) => {
        const i = params.findIndex(([n]) => n === k);
        if (i >= 0) params[i][1] = v; else params.push([k, v]);
      };
      let hst = host, prt = port;
      const build = () => scheme + "://" + hst + ":" + prt + joinQuery(params);
      if (scheme === "srt") {
        const mode = get("mode") || "caller";
        const modeSel = h("select", { id: "h-" + path + "-mode" },
          [["listener", path.startsWith("outputs") ? "Listener (wait for the receiver)" : "Listener (wait for the sender)"], ["caller", "Caller (connect out)"]].map(([v, t]) => h("option", { value: v, selected: v === mode }, t)));
        modeSel.addEventListener("change", () => { put("mode", modeSel.value); commit(build()); });
        g.append(h("div", { class: "field" }, h("label", { for: "h-" + path + "-mode" }, "Mode"), modeSel));
      }
      mk(scheme === "srt" && get("mode") !== "caller" ? "Listen address" : "Host", host, { class: "mono" }, (i) => { hst = i.value; return build(); },
        isOutput ? null : (scheme === "srt" ? "0.0.0.0 = any interface" : "Multicast group or local address"));
      mk("Port", port, { inputmode: "numeric" }, (i) => { prt = i.value.replace(/\D/g, ""); return build(); });
      if (scheme === "srt") {
        const pass = get("passphrase");
        const masked = pass === "***";
        mk("Passphrase", masked ? "" : pass, { type: "password", placeholder: masked ? "Saved — leave blank to keep" : "Optional (10–79 characters)" },
          (i) => { put("passphrase", i.value === "" && masked ? "***" : i.value); return build(); }, "Encrypts the SRT link.");
      }
    }
    g.append(h("div", { class: "field wide" }, h("label", { for: urlId }, "URL"), urlInput, errSlot(path)));
    box.append(g);
  }
  urlInput.addEventListener("change", () => { setPath(S.draft, path, urlInput.value.trim()); onChange(); draw(); });
  urlInput.addEventListener("input", () => { setPath(S.draft, path, urlInput.value.trim()); onChange(); });
  draw();
  return box;
}

function renderInput(card) {
  card.append(h("div", { class: "endpoint" }, endpointEditor("input.url", ["srt", "udp", "rtp"], false)));
}

function renderOutputs(card) {
  const list = h("div");
  S.draft.outputs.forEach((o, i) => {
    list.append(h("div", { class: "endpoint" },
      h("div", { class: "endpoint-head" },
        h("span", { class: "label" }, "Output " + (i + 1)),
        h("button", { type: "button", class: "icon", "aria-label": "Remove output " + (i + 1), onclick: () => { S.draft.outputs.splice(i, 1); onChange(); renderSettings(); } }, "Remove")),
      endpointEditor(`outputs[${i}].url`, ["srt", "udp", "rtmp", "rtmps"], true)));
  });
  card.append(list, errSlot("outputs"),
    h("button", { type: "button", onclick: () => { S.draft.outputs.push({ url: "udp://127.0.0.1:5000" }); onChange(); renderSettings(); } }, "+ Add output"));
}

function renderLanguages(card) {
  const rows = S.draft.languages.map((l, i) => {
    const at = (f) => `languages[${i}].${f}`;
    const code = h("input", { type: "text", maxlength: 2, size: 3, class: "mono", "aria-label": `Language ${i + 1} code` });
    code.value = l.code;
    bindInput(code, at("code"), "text");
    code.addEventListener("input", () => { name.textContent = LANG_NAMES[code.value] || ""; });
    const name = h("span", { class: "help" }, LANG_NAMES[l.code] || "");
    const src = h("input", { type: "radio", name: "source-lang", "aria-label": `Language ${i + 1} is the spoken language` });
    src.checked = l.source;
    src.addEventListener("change", () => { S.draft.languages.forEach((x, j) => (x.source = j === i)); onChange(); });
    const cc = h("select", { "aria-label": `Language ${i + 1} CEA-608 channel` },
      [["", "—"], ["cc1", "CC1"], ["cc2", "CC2"], ["cc3", "CC3"], ["cc4", "CC4"]].map(([v, t]) => h("option", { value: v, selected: (l.cc608 || "") === v }, t)));
    cc.addEventListener("change", () => { l.cc608 = cc.value || null; onChange(); });
    cc.dataset.path = at("cc608");
    const svc = h("select", { "aria-label": `Language ${i + 1} CEA-708 service` },
      [["", "—"], 1, 2, 3, 4, 5, 6].map((v) => Array.isArray(v) ? h("option", { value: "" , selected: l.cea708_service == null }, "—") : h("option", { value: v, selected: l.cea708_service === v }, "Service " + v)));
    svc.addEventListener("change", () => { l.cea708_service = svc.value ? Number(svc.value) : null; onChange(); });
    svc.dataset.path = at("cea708_service");
    const pri = h("input", { type: "number", min: 0, max: 255, style: "width:70px", "aria-label": `Language ${i + 1} priority` });
    pri.value = l.priority;
    bindInput(pri, at("priority"), "number");
    const del = h("button", { type: "button", class: "icon", "aria-label": `Remove language ${l.code}`, onclick: () => { S.draft.languages.splice(i, 1); onChange(); renderSettings(); } }, "Remove");
    return h("tr", {},
      h("td", {}, code, " ", name, errSlot(at("code"))),
      h("td", {}, src),
      h("td", {}, cc, errSlot(at("cc608"))),
      h("td", {}, svc, errSlot(at("cea708_service"))),
      h("td", {}, pri),
      h("td", {}, del));
  });
  card.append(h("div", { class: "table-wrap" }, h("table", {},
    h("thead", {}, h("tr", {}, h("th", {}, "Code"), h("th", {}, "Spoken"), h("th", {}, "CEA-608"), h("th", {}, "CEA-708"), h("th", {}, "Priority"), h("th", {}, h("span", { class: "sr-only" }, "Actions")))),
    h("tbody", {}, rows))),
  h("p", { class: "help" }, "Priority: lower numbers are kept longest when shedding load. CC1/CC3 are the usual 608 channels (field 1 and 2)."),
  errSlot("languages"),
  h("button", { type: "button", onclick: () => { S.draft.languages.push({ code: "", source: false, cc608: null, cea708_service: null, priority: S.draft.languages.length }); onChange(); renderSettings(); } }, "+ Add language"));
}

const OPEN_GROUPS = new Set();

function renderSettings() {
  const form = $("#settings-form");
  const focusId = document.activeElement && document.activeElement.id;
  form.replaceChildren();
  for (const s of SECTIONS) {
    // Each group collapses on its own; which ones are open survives re-renders.
    const card = h("details", { class: "card settings-group", id: "sec-" + s.id, open: OPEN_GROUPS.has(s.id) },
      h("summary", {}, h("h2", { id: "h-" + s.id }, s.title), s.lede ? h("span", { class: "lede" }, s.lede) : null));
    card.addEventListener("toggle", () => {
      if (card.open) OPEN_GROUPS.add(s.id); else OPEN_GROUPS.delete(s.id);
      updateSavebar();
    });
    if (s.custom) s.custom(card);
    else card.append(h("div", { class: "grid" }, s.fields.map(renderField)));
    form.append(card);
  }
  showIssues();
  if (focusId && document.getElementById(focusId)) document.getElementById(focusId).focus();
  markSection();
}

function showIssues() {
  $$("[aria-invalid]").forEach((e) => e.removeAttribute("aria-invalid"));
  $$(".err-msg").forEach((e) => (e.textContent = ""));
  const box = $("#issues");
  box.replaceChildren();
  if (!S.issues.length) return;
  const items = S.issues.map((i) => {
    const slot = document.querySelector(`[data-err="${CSS.escape(i.path)}"]`);
    if (slot) slot.textContent = slot.textContent ? slot.textContent + " " + i.message : i.message;
    const input = document.querySelector(`[data-path="${CSS.escape(i.path)}"]`);
    if (input) input.setAttribute("aria-invalid", "true");
    reveal(input || slot);
    const target = input || (slot && slot.closest(".card"));
    return h("li", {}, target
      ? h("button", { type: "button", class: "link", onclick: () => { reveal(target); target.scrollIntoView({ block: "center" }); if (input) input.focus(); } }, h("code", {}, i.path || "config"))
      : h("code", {}, i.path || "config"), " — ", i.message);
  });
  box.append(h("div", { class: "banner err", role: "alert" },
    h("p", {}, `${S.issues.length} problem${S.issues.length > 1 ? "s" : ""} to fix before saving:`, h("ul", {}, items))));
}

async function loadConfig() {
  const r = await api("GET", "/api/config");
  if (!r.ok) return;
  S.saved = r.data;
  S.draft = clone(r.data);
  S.issues = [];
  renderSettings();
  onChange();
  renderLanes();
}

async function save() {
  $("#save").disabled = true;
  const r = await api("PUT", "/api/config", S.draft);
  if (r.status === 422) {
    S.issues = r.data.issues || [];
    showIssues();
    $("#issues").scrollIntoView({ block: "start" });
    onChange();
    return;
  }
  if (!r.ok) { banner("err", "Could not save: " + ((r.data && r.data.error) || r.status)); onChange(); return; }
  S.saved = r.data.config;
  S.draft = clone(r.data.config);
  S.issues = [];
  renderSettings();
  onChange();
  renderLanes();
  const rep = r.data.report;
  lastReport = rep;
  flash("Saved." + (rep.live.length && !rep.restart.length ? " Changes are in effect." : ""));
  refreshStatus();
}

// ------------------------------------------------------------ banners

let lastReport = null;
let flashTimer = null;
function flash(text) {
  const b = $("#flash") || h("div", { id: "flash", class: "banner ok", role: "status" });
  b.textContent = text;
  if (!b.isConnected) $("#banners").prepend(b);
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => b.remove(), 4000);
}
function banner(kind, text) {
  $("#banners").append(h("div", { class: "banner " + kind, role: "alert" }, h("p", {}, text),
    h("button", { type: "button", class: "icon", onclick: (e) => e.target.closest(".banner").remove() }, "Dismiss")));
}
function renderRestartBanners(st) {
  $$(".restart-banner").forEach((b) => b.remove());
  if (st.restart_pending && st.restart_pending.length) {
    $("#banners").append(h("div", { class: "banner warn restart-banner", role: "status" },
      h("p", {}, "Saved changes wait for a pipeline restart: ", st.restart_pending.map((p) => h("code", {}, p + " ")), ". Restarting interrupts the output briefly."),
      h("button", { type: "button", onclick: restart }, "Restart now")));
  }
  if (st.server_restart_pending && st.server_restart_pending.length) {
    $("#banners").append(h("div", { class: "banner warn restart-banner", role: "status" },
      h("p", {}, "Restart multi serve to use the new web address (", st.server_restart_pending.join(", "), ").")));
  }
}

let warnKey = "";
function renderWarningBanners(st) {
  const key = JSON.stringify(st.warnings || []);
  if (key === warnKey) return;
  warnKey = key;
  $$(".warn-banner").forEach((b) => b.remove());
  for (const w of st.warnings || []) {
    const input = w.path && document.querySelector(`[data-path="${CSS.escape(w.path)}"]`);
    const where = w.path === "audio" || !w.path ? null
      : input ? h("button", { type: "button", class: "link", onclick: () => { reveal(input); input.scrollIntoView({ block: "center" }); input.focus(); } }, h("code", {}, w.path))
      : h("code", {}, w.path);
    $("#banners").append(h("div", { class: "banner warn warn-banner", role: "status" },
      h("p", {}, where ? [where, " — ", w.message] : w.message)));
  }
}

// ------------------------------------------------------------ status

const STATE_TEXT = { stopped: "Stopped", starting: "Starting", running: "Running", stopping: "Stopping", failed: "Failed" };
const STATE_CLASS = { stopped: "", starting: "warn", running: "ok", stopping: "warn", failed: "err" };
const fmt = (n) => (n == null ? "–" : Number(n).toLocaleString());
function dur(s) {
  if (s == null) return "";
  const hh = Math.floor(s / 3600), mm = Math.floor((s % 3600) / 60), ss = s % 60;
  return (hh ? hh + "h " : "") + (hh || mm ? mm + "m " : "") + ss + "s";
}
function pill(text, cls) { return h("span", { class: "pill " + (cls || "") }, text); }

let prev = null;
function renderStatus(st) {
  S.status = st;
  const active = st.state === "running" || st.state === "starting";
  $("#state-pill").className = "pill " + STATE_CLASS[st.state];
  $("#state-pill").textContent = STATE_TEXT[st.state];
  $$(".startstop").forEach((b) => {
    b.textContent = active ? "Stop" : st.state === "stopping" ? "Stopping…" : "Start";
    b.className = "startstop " + (active ? "danger" : "primary");
    b.disabled = st.state === "stopping";
  });
  $("#st-state").textContent = STATE_TEXT[st.state];
  $("#st-sub").textContent = active ? "Up " + dur(st.uptime_s) : st.state === "failed" ? "See recent errors" : "Pipeline not running";
  renderRestartBanners(st);
  renderWarningBanners(st);

  const m = st.media;
  const rate = (k) => (prev && prev.media && m ? Math.max(0, m[k] - prev.media[k]) : null);
  $("#t-input").replaceChildren(m ? pill(m.input_live ? "Live" : "No signal", m.input_live ? "ok" : "err") : "–");
  $("#t-input-d").textContent = m ? `${m.input_restarts} restart${m.input_restarts === 1 ? "" : "s"}` : " ";
  $("#t-in").textContent = m ? fmt(m.frames_in) : "–";
  $("#t-out").textContent = m ? fmt(m.frames_out) : "–";
  const rin = rate("frames_in"), rout = rate("frames_out");
  $("#t-in-d").textContent = rin != null ? rin + " fps" : " ";
  $("#t-out-d").textContent = rout != null ? rout + " fps" : " ";
  const silentWarn = m && m.input_live && m.audio_silent_s != null && m.audio_silent_s >= 10;
  const rms = m ? m.audio_rms_dbfs : null;
  $("#t-audio").textContent = rms == null ? "–" : rms <= -99 ? "Silent" : rms.toFixed(0) + " dBFS";
  const bar = $("#t-audio-bar");
  bar.style.width = rms == null ? "0%" : Math.max(0, Math.min(100, ((rms + 60) / 60) * 100)) + "%";
  bar.className = silentWarn ? "warn" : "";
  $("#t-audio-d").textContent = m && m.audio_peak_dbfs != null
    ? (silentWarn ? "silent for " + dur(Math.round(m.audio_silent_s)) : "peak " + m.audio_peak_dbfs.toFixed(0) + " dBFS")
    : " ";
  $("#t-lag").textContent = st.caption_lag_ms != null ? (st.caption_lag_ms / 1000).toFixed(2) + " s" : "–";
  $("#t-cc").textContent = m ? fmt(m.caption_frames) : "–";
  $("#t-cc-d").textContent = m && m.caption_errors ? m.caption_errors + " encoder errors" : " ";

  const table = (heads, rows, empty) => rows.length
    ? h("table", {}, h("thead", {}, h("tr", {}, heads.map(([t, c]) => h("th", { class: c }, t)))), h("tbody", {}, rows))
    : h("p", { class: "empty" }, empty);
  $("#st-outputs").replaceChildren(table([["URL"], ["State"], ["Errors", "num"], ["Starts", "num"]],
    (m ? m.outputs : []).map((o) => h("tr", {}, h("td", { class: "mono" }, o.url), h("td", {}, pill(o.running ? "Up" : "Down", o.running ? "ok" : "err")),
      h("td", { class: "num" }, fmt(o.errors)), h("td", { class: "num" }, fmt(o.starts)))), "Not running."));
  const WNAME = { asr: "Speech recognition", mt: "Translation" };
  const WCLS = { ready: "ok", starting: "warn", restarting: "warn", failed: "err", stopped: "" };
  $("#st-workers").replaceChildren(table([["Worker"], ["State"], ["Restarts", "num"], ["Last error"]],
    st.workers.map((w) => h("tr", {}, h("td", {}, WNAME[w.name] || w.name), h("td", {}, pill(w.state, WCLS[w.state])),
      h("td", { class: "num" }, fmt(w.restarts)), h("td", { class: "mono" }, w.last_error || "—"))), "Not running."));
  $("#st-lanes").replaceChildren(table([["Language"], ["Sent", "num"], ["Dropped", "num"], ["Queued", "num"]],
    (m ? m.lanes : []).map((l) => h("tr", {}, h("td", {}, l.lang.toUpperCase() + " ", h("span", { class: "help" }, LANG_NAMES[l.lang] || "")),
      h("td", { class: "num" }, fmt(l.pushed)), h("td", { class: "num" }, fmt(l.dropped)), h("td", { class: "num" }, fmt(l.queued)))), "Not running."));
  $("#st-gpu").replaceChildren(...(st.gpu && st.gpu.length ? st.gpu.map((g) => {
    const pct = g.memory_total_mb ? Math.round((100 * g.memory_used_mb) / g.memory_total_mb) : 0;
    return h("div", { style: "margin-bottom:10px" },
      h("div", {}, h("strong", {}, `GPU ${g.index}`), " ", g.name),
      h("div", { class: "help" }, `VRAM ${fmt(g.memory_used_mb)} / ${fmt(g.memory_total_mb)} MB (${pct}%) · load ${g.utilization_pct}%`),
      h("div", { class: "meter", role: "meter", "aria-valuemin": 0, "aria-valuemax": 100, "aria-valuenow": pct, "aria-label": "VRAM used" }, h("i", { style: `width:${pct}%` })));
  }) : [h("p", { class: "empty" }, "No NVIDIA GPU found (nvidia-smi).")]));
  $("#st-errors").replaceChildren(...(st.errors.length ? st.errors.map((e) => h("li", {},
    h("time", { datetime: new Date(e.at_ms).toISOString() }, new Date(e.at_ms).toLocaleTimeString()),
    h("span", { class: "src" }, e.source), h("span", {}, e.message))) : [h("li", { class: "empty", style: "display:block" }, "None.")]));
  prev = st;
}

async function refreshStatus() {
  const r = await api("GET", "/api/status");
  if (r.ok) renderStatus(r.data);
}

async function startStop() {
  const active = S.status && (S.status.state === "running" || S.status.state === "starting");
  if (!active && dirty() && !confirm("You have unsaved settings. Start with the saved settings?")) return;
  $$(".startstop").forEach((b) => (b.disabled = true));
  const r = await api("POST", active ? "/api/stop" : "/api/start");
  if (r.status === 422) {
    S.issues = r.data.issues || [];
    showIssues();
    openSettings();
  } else if (!r.ok) {
    banner("err", "Could not start: " + ((r.data && r.data.error) || r.status));
  }
  if (r.ok) renderStatus(r.data); else refreshStatus();
}

async function restart() {
  $$(".startstop").forEach((b) => (b.disabled = true));
  await api("POST", "/api/stop");
  const r = await api("POST", "/api/start");
  if (!r.ok) banner("err", "Could not start: " + ((r.data && r.data.error) || r.status));
  refreshStatus();
}

// ------------------------------------------------------------ live captions

const LANE_LINES = 12;
function renderLanes() {
  const grid = $("#lanes");
  const langs = (S.saved ? S.saved.languages : []);
  const have = new Set($$(".lane", grid).map((l) => l.dataset.lang));
  const want = langs.map((l) => l.code);
  if (have.size === want.length && want.every((c) => have.has(c))) return;
  grid.replaceChildren(...langs.map((l) => {
    const where = [l.cc608 && l.cc608.toUpperCase(), l.cea708_service && "708 service " + l.cea708_service].filter(Boolean).join(" · ");
    return h("section", { class: "lane", "data-lang": l.code, "aria-label": (LANG_NAMES[l.code] || l.code) + " captions" },
      h("div", { class: "lane-head" }, h("strong", {}, l.code.toUpperCase()), h("span", {}, LANG_NAMES[l.code] || ""),
        l.source ? pill("spoken") : null, h("span", { class: "where" }, where)),
      h("div", { class: "lane-body", "aria-live": "off", tabindex: 0 }));
  }));
}
function appendCaption(ev) {
  const lane = document.querySelector(`.lane[data-lang="${CSS.escape(ev.lang)}"] .lane-body`);
  if (!lane) return;
  let last = lane.lastElementChild;
  if (ev.new_row || !last) {
    last = h("p");
    lane.append(last);
    while (lane.children.length > LANE_LINES) lane.firstElementChild.remove();
  }
  last.textContent = last.textContent ? last.textContent + " " + ev.text : ev.text;
  lane.scrollTop = lane.scrollHeight;
}

function connectEvents() {
  const es = new EventSource("/api/events");
  es.onopen = () => ($("#conn").textContent = "Live");
  es.onerror = () => ($("#conn").textContent = "Reconnecting…");
  es.onmessage = (m) => {
    let ev;
    try { ev = JSON.parse(m.data); } catch (_) { return; }
    if (ev.type === "stats") renderStatus(ev);
    else if (ev.type === "caption") appendCaption(ev);
  };
}

// ------------------------------------------------------------ layout
// One page: status, live captions, then settings in a collapsible section.
// The save bar shows while settings are open or there are unsaved changes.

// Opens the settings group that holds `el` (a field or error slot).
function reveal(el) {
  const group = el && el.closest("details.settings-group");
  if (group && !group.open) group.open = true;
}
// Brings the settings section into view; groups with problems are opened by showIssues.
function openSettings() {
  $("#settings").scrollIntoView({ block: "start" });
}
// Sidebar: one link per settings group (opens it), and the link for the
// section in view is marked with aria-current.
let markSection = () => {};
function buildSideNav() {
  $("#nav-groups").replaceChildren(...SECTIONS.map((sec) =>
    h("li", {}, h("a", { href: "#sec-" + sec.id, onclick: () => { const g = $("#sec-" + sec.id); if (g) g.open = true; } }, sec.title))));
  // Current section = the last one whose heading has scrolled past the header.
  const links = $$(".side-nav > a");
  const ids = ["status", "captions", "settings"];
  const mark = () => {
    let cur = ids[0];
    for (const id of ids) if ($("#" + id).getBoundingClientRect().top <= 100) cur = id;
    const root = document.documentElement;
    if (window.scrollY > 0 && window.innerHeight + window.scrollY >= root.scrollHeight - 2) cur = ids[ids.length - 1];
    links.forEach((a) => (a.getAttribute("href") === "#" + cur ? a.setAttribute("aria-current", "true") : a.removeAttribute("aria-current")));
  };
  markSection = mark;
  window.addEventListener("scroll", mark, { passive: true });
  window.addEventListener("resize", mark);
  mark();
}

function updateSavebar() {
  const anyOpen = $$("details.settings-group[open]").length > 0;
  $("#savebar").hidden = !(anyOpen || dirty());
}

// ------------------------------------------------------------ boot

$("#save").addEventListener("click", save);
$("#revert").addEventListener("click", () => { S.draft = clone(S.saved); S.issues = []; renderSettings(); onChange(); });
$("#load-defaults").addEventListener("click", async () => {
  const r = await api("GET", "/api/config/default");
  if (!r.ok) return;
  const d = r.data;
  // Keep the saved endpoints and web settings; reset everything else.
  d.input = S.draft.input; d.outputs = S.draft.outputs; d.web = S.draft.web;
  S.draft = d;
  renderSettings();
  onChange();
  flash("Defaults loaded (input, outputs and web kept). Review, then Save.");
});
$("#settings-form").addEventListener("submit", (e) => { e.preventDefault(); if (dirty()) save(); });
document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "s" && $$("details.settings-group[open]").length > 0) { e.preventDefault(); if (dirty()) save(); }
});
$$(".startstop").forEach((b) => b.addEventListener("click", startStop));
$("#clear-captions").addEventListener("click", () => $$(".lane-body").forEach((l) => l.replaceChildren()));
window.addEventListener("beforeunload", (e) => { if (S.draft && dirty()) e.preventDefault(); });

buildSideNav();
updateSavebar();
loadConfig().then(refreshStatus);
connectEvents();

// ------------------------------------------------------------ sign-in (WP9)
api("GET", "/api/me").then((r) => {
  if (!r.ok || !r.data) return;
  $("#logout").hidden = r.data.via !== "session";
  if (!r.data.password_set) {
    $("#banners").append(h("div", { class: "banner warn", role: "status" },
      h("p", {}, "No password is set, so anyone using this computer can change these settings. ",
        h("a", { href: "/setup" }, "Set a password"), " (or run multi passwd).")));
  }
}).catch(() => {});
