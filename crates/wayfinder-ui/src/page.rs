pub const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Wayfinder</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #f4efe6; --surface: #fbf8f2; --surface-2: #efe8da;
    --text: #1b1f1d; --muted: #5c635f; --hairline: #ddd4c4;
    --accent: #0f7d73; --accent-hover: #0c655d; --accent-weak: #d8ede9; --on-accent: #ffffff;
    --cloud: #9a5b15;
    --ok: #1a7d4b; --ok-weak: #d9efe1; --err: #c0392b; --err-weak: #f7e0dc;
    --bar: #0f7d73; --track: #e4dccc;
    --radius: 10px; --radius-sm: 6px; --radius-pill: 999px;
    --sp-1: .25rem; --sp-2: .5rem; --sp-3: .75rem; --sp-4: 1rem; --sp-5: 1.5rem;
    --shadow-sm: 0 1px 2px rgba(20,25,22,.08);
    --ring: 0 0 0 3px rgba(15,125,115,.35);
    --font-ui: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
    --font-mono: ui-monospace, "SF Mono", "Cascadia Code", "JetBrains Mono", Menlo, Consolas, monospace;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #0e1614; --surface: #15211e; --surface-2: #0a100f;
      --text: #eef2ee; --muted: #9aa6a0; --hairline: #28332f;
      --accent: #2bb6a6; --accent-hover: #46c8b9; --accent-weak: #142e2a; --on-accent: #06201d;
      --cloud: #d99a4e;
      --ok: #43c483; --ok-weak: #12281d; --err: #f08a7d; --err-weak: #2c1714;
      --bar: #2bb6a6; --track: #20302c;
      --shadow-sm: 0 1px 2px rgba(0,0,0,.4);
      --ring: 0 0 0 3px rgba(43,182,166,.45);
    }
  }
  * { box-sizing: border-box; }
  body { font: 15px/1.55 var(--font-ui); color: var(--text); background: var(--bg);
         margin: 0; padding: var(--sp-5); max-width: 920px; margin-inline: auto; }
  @media (max-width: 640px) { body { padding: var(--sp-4); } }

  .brand { display: flex; align-items: baseline; gap: var(--sp-3); flex-wrap: wrap;
           padding-bottom: var(--sp-4); margin-bottom: var(--sp-2);
           border-bottom: 1px solid var(--hairline); }
  .brand h1 { font-size: 1.35rem; font-weight: 700; letter-spacing: -.01em;
              line-height: 1.1; margin: 0; }
  .brand .tag { font: 500 .8rem/1.3 var(--font-mono); color: var(--muted);
                letter-spacing: .02em; margin-left: auto; }
  @media (max-width: 640px) { .brand .tag { margin-left: 0; width: 100%; } }

  nav { display: flex; gap: var(--sp-1); margin: var(--sp-5) 0; flex-wrap: wrap;
        border-bottom: 1px solid var(--hairline); }
  nav button { font: 600 .92rem var(--font-ui); color: var(--muted); cursor: pointer;
               padding: var(--sp-3) var(--sp-4); border: 0; background: transparent;
               border-bottom: 2px solid transparent; margin-bottom: -1px;
               border-radius: var(--radius-sm) var(--radius-sm) 0 0;
               transition: color .15s, background .15s; }
  nav button:hover { color: var(--text); background: var(--accent-weak); }
  nav button.on { color: var(--accent); border-bottom-color: var(--accent); }
  nav button:focus-visible { outline: none; box-shadow: var(--ring); }

  section { display: none; }
  section.on { display: block; }
  .card { background: var(--surface); border: 1px solid var(--hairline);
          border-radius: var(--radius); box-shadow: var(--shadow-sm); padding: var(--sp-5); }
  .card > :first-child { margin-top: 0; }
  p.muted { margin-top: 0; }

  .row { display: flex; gap: var(--sp-4); align-items: center; margin: var(--sp-4) 0; flex-wrap: wrap; }
  .muted { color: var(--muted); }
  label { font-weight: 600; font-size: .9rem; }

  textarea, input[type=text], input:not([type]), select {
    width: 100%; box-sizing: border-box; font: inherit; color: var(--text);
    background: var(--surface); border: 1px solid var(--hairline);
    border-radius: var(--radius-sm); padding: var(--sp-3);
    transition: border-color .15s, box-shadow .15s; }
  textarea { font: .82rem/1.5 var(--font-mono); resize: vertical; }
  #prompt, #dataset, #prompts { min-height: 140px; }
  #toml { min-height: 240px; }
  textarea:focus, input:focus, select:focus { outline: none;
    border-color: var(--accent); box-shadow: var(--ring); }
  #models { max-width: 340px; }
  select { width: auto; cursor: pointer; padding-right: var(--sp-5);
    appearance: none; -webkit-appearance: none;
    background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='12' height='12' viewBox='0 0 12 12'%3E%3Cpath d='M2 4l4 4 4-4' stroke='%230f7d73' stroke-width='1.6' fill='none' stroke-linecap='round'/%3E%3C/svg%3E");
    background-repeat: no-repeat; background-position: right var(--sp-3) center; }

  button.act { font: 600 .9rem var(--font-ui); cursor: pointer;
    padding: var(--sp-2) var(--sp-4); border-radius: var(--radius-sm);
    border: 1px solid var(--hairline); background: var(--surface); color: var(--text);
    transition: background .15s, border-color .15s, box-shadow .15s, transform .02s; }
  button.act:hover { background: var(--accent-weak); border-color: var(--accent); }
  button.act:active { transform: translateY(1px); }
  button.act:focus-visible { outline: none; box-shadow: var(--ring); }
  button.act:disabled { opacity: .5; cursor: not-allowed; }
  button.act.primary { background: var(--accent); border-color: var(--accent); color: var(--on-accent); }
  button.act.primary:hover { background: var(--accent-hover); border-color: var(--accent-hover); }

  input[type=range] { flex: 1; appearance: none; -webkit-appearance: none;
    height: 22px; background: transparent; cursor: pointer; }
  input[type=range]::-webkit-slider-runnable-track { height: 6px;
    border-radius: var(--radius-pill); background: var(--track); }
  input[type=range]::-webkit-slider-thumb { -webkit-appearance: none;
    width: 18px; height: 18px; margin-top: -6px; border-radius: 50%;
    background: var(--accent); border: 2px solid var(--surface);
    box-shadow: var(--shadow-sm); transition: transform .1s; }
  input[type=range]:hover::-webkit-slider-thumb { transform: scale(1.1); }
  input[type=range]:focus-visible { outline: none; }
  input[type=range]:focus-visible::-webkit-slider-thumb { box-shadow: var(--ring); }
  input[type=range]::-moz-range-track { height: 6px;
    border-radius: var(--radius-pill); background: var(--track); }
  input[type=range]::-moz-range-thumb { width: 18px; height: 18px;
    border: 2px solid var(--surface); border-radius: 50%;
    background: var(--accent); box-shadow: var(--shadow-sm); }
  input[type=range]:focus-visible::-moz-range-thumb { box-shadow: var(--ring); }
  output { font-variant-numeric: tabular-nums; }

  .rec { display: inline-flex; align-items: center; gap: var(--sp-2);
    font: 700 1.05rem var(--font-ui); padding: var(--sp-2) var(--sp-4);
    border-radius: var(--radius-pill); background: var(--accent-weak);
    color: var(--accent); letter-spacing: .01em; }
  #score { color: var(--muted); font-size: .9rem; font-variant-numeric: tabular-nums; }

  .tier { font: 600 .82rem var(--font-mono); padding: var(--sp-1) var(--sp-3);
    border-radius: var(--radius-pill); background: var(--surface-2);
    color: var(--muted); border: 1px solid var(--hairline); font-variant-numeric: tabular-nums; }
  .tier.on { background: var(--accent); color: var(--on-accent); border-color: var(--accent); }

  .track { background: var(--track); border-radius: var(--radius-pill);
    flex: 1; min-width: 60px; height: 10px; overflow: hidden; display: block; }
  td.track { padding: 0; vertical-align: middle; }
  span.track { height: 10px; }
  .bar { height: 10px; background: var(--bar); border-radius: var(--radius-pill); display: block; }

  table { width: 100%; border-collapse: collapse; margin-top: var(--sp-3);
    font-variant-numeric: tabular-nums; }
  th, td { text-align: left; padding: var(--sp-2) var(--sp-3);
    border-bottom: 1px solid var(--hairline); }
  th { font: 600 .78rem var(--font-ui); color: var(--muted);
    text-transform: uppercase; letter-spacing: .04em; }
  tbody tr:last-child td { border-bottom: 0; }
  tbody tr:hover { background: var(--accent-weak); }

  pre { background: var(--surface-2); color: var(--text);
    border: 1px solid var(--hairline); border-radius: var(--radius-sm);
    padding: var(--sp-4); overflow: auto; font: .82rem/1.5 var(--font-mono); }
  code { background: var(--surface-2); border-radius: 4px; padding: .1em .4em;
    font: .85em var(--font-mono); }

  .ok, .err { display: inline-block; font-weight: 600; font-size: .85rem;
    padding: var(--sp-1) var(--sp-3); border-radius: var(--radius-pill); }
  .ok { color: var(--ok); background: var(--ok-weak); }
  .err { color: var(--err); background: var(--err-weak); white-space: pre-wrap; }
  .ok:empty, .err:empty { display: none; padding: 0; }

  @media (prefers-reduced-motion: reduce) { * { transition: none !important; } }
</style>
</head>
<body>
  <header class="brand">
    <h1>Wayfinder</h1>
    <span class="tag">Deterministic. Calibrated. No&nbsp;RAG, no&nbsp;guessing.</span>
  </header>
  <nav>
    <button data-tab="explain" class="on">Explain</button>
    <button data-tab="calibrate">Calibrate</button>
    <button data-tab="configure">Configure</button>
    <button data-tab="onboard">Onboard</button>
  </nav>

  <section id="explain" class="on">
  <div class="card">
    <textarea id="prompt" placeholder="Paste a prompt to score it..."></textarea>
    <div class="row">
      <label>Threshold override: <output id="tval">off</output></label>
      <input type="range" id="threshold" min="0" max="1" step="0.01" value="-1">
      <button class="act" id="clear">use config</button>
    </div>
    <div class="row"><span class="rec" id="rec">—</span><span class="muted" id="score"></span></div>
    <div id="tiers" class="row"></div>
    <table><thead><tr><th>Feature</th><th>Value</th><th>Norm</th><th>Weight</th>
      <th>Contribution</th><th></th></tr></thead><tbody id="breakdown"></tbody></table>
  </div>
  </section>

  <section id="calibrate">
  <div class="card">
    <p class="muted">Paste a labeled dataset, one JSON object per line:
      <code>{"text": "...", "label": "local"}</code></p>
    <textarea id="dataset" placeholder='{"text": "summarise this", "label": "local"}'></textarea>
    <div class="row">
      <label>Mode <select id="mode">
        <option value="threshold">threshold</option>
        <option value="tiers">tiers</option>
        <option value="classifier">classifier</option>
      </select></label>
      <input id="models" placeholder="models order (optional, comma-separated)">
      <button class="act primary" id="runcal">Calibrate</button>
    </div>
    <div id="calsummary" class="muted"></div>
    <div id="calerr" class="err"></div>
    <div id="curve"></div>
    <pre id="calout" hidden></pre>
    <button class="act" id="tocfg" hidden>Send to Configure →</button>
  </div>
  </section>

  <section id="configure">
  <div class="card">
    <p class="muted">Edit <code>wayfinder-router.toml</code>. Keys are never stored here —
      a gateway model names an <code>api_key_env</code> and the secret stays in the
      environment.</p>
    <textarea id="toml"></textarea>
    <div class="row">
      <button class="act" id="validate">Validate</button>
      <button class="act primary" id="save">Save</button>
      <span id="cfgstatus"></span>
    </div>
  </div>
  </section>

  <section id="onboard">
  <div class="card">
    <p class="muted">A/B a local vs hosted model on sample prompts, judge each, and
      record labels. Needs two <code>[gateway.models]</code> and the
      <code>[gateway]</code> extra. Arms: <span id="arms">—</span> · labels so far:
      <span id="lblcount">0</span></p>
    <textarea id="prompts" placeholder='one prompt per line, or {"text": "..."}'></textarea>
    <div class="row">
      <button class="act primary" id="startob">Start</button>
      <span class="muted" id="obprogress"></span>
    </div>
    <div id="obcurrent" hidden>
      <pre id="obprompt"></pre>
      <div class="row" id="obarms"></div>
      <div class="row" id="objudge"></div>
    </div>
    <div id="oberr" class="err"></div>
    <div class="row">
      <button class="act" id="obcal">Calibrate from log →</button>
      <button class="act primary" id="obrecal">Recalibrate &amp; save</button>
      <span id="obrecalstatus" class="muted"></span>
    </div>
  </div>
  </section>

<script>
const $ = id => document.getElementById(id);
async function post(url, body) {
  const r = await fetch(url, {method: "POST", headers: {"Content-Type": "application/json"},
    body: JSON.stringify(body)});
  return {ok: r.ok, data: await r.json()};
}

// --- tabs ---
document.querySelectorAll("nav button").forEach(b => b.addEventListener("click", () => {
  document.querySelectorAll("nav button").forEach(x => x.classList.remove("on"));
  document.querySelectorAll("section").forEach(x => x.classList.remove("on"));
  b.classList.add("on");
  $(b.dataset.tab).classList.add("on");
  if (b.dataset.tab === "configure" && !$("toml").value) loadConfig();
  if (b.dataset.tab === "onboard") loadOnboardState();
}));

// --- explain ---
let timer;
function scheduleScore() { clearTimeout(timer); timer = setTimeout(score, 150); }
async function score() {
  const t = parseFloat($("threshold").value);
  const threshold = t >= 0 ? t : null;
  $("tval").textContent = threshold === null ? "off" : threshold.toFixed(2);
  const {data} = await post("/api/score", {prompt: $("prompt").value, threshold});
  $("rec").textContent = data.recommendation;
  $("score").textContent = "score " + data.score.toFixed(2) + " · " + data.mode;
  const tiers = $("tiers"); tiers.innerHTML = "";
  (data.tiers || []).forEach(t => {
    const el = document.createElement("span");
    el.className = "tier" + (t.model === data.recommendation ? " on" : "");
    el.textContent = "≥ " + t.min_score.toFixed(2) + " " + t.model;
    tiers.appendChild(el);
  });
  if (data.models) { const el = document.createElement("span"); el.className = "muted";
    el.textContent = "candidates: " + data.models.join(", "); tiers.appendChild(el); }
  const body = $("breakdown"); body.innerHTML = "";
  const max = Math.max(0.0001, ...data.contributions.map(c => c.contribution));
  data.contributions.forEach(c => {
    const tr = document.createElement("tr");
    const pct = (100 * c.contribution / max).toFixed(0);
    tr.innerHTML = `<td>${c.name}</td><td>${c.value}</td><td>${c.normalized.toFixed(2)}</td>` +
      `<td>${c.weight}</td><td>${c.contribution.toFixed(3)}</td>` +
      `<td class="track"><div class="bar" style="width:${pct}%"></div></td>`;
    body.appendChild(tr);
  });
}
$("prompt").addEventListener("input", scheduleScore);
$("threshold").addEventListener("input", scheduleScore);
$("clear").addEventListener("click", () => { $("threshold").value = -1; score(); });

// --- calibrate ---
$("runcal").addEventListener("click", async () => {
  $("calerr").textContent = ""; $("curve").innerHTML = "";
  const {ok, data} = await post("/api/calibrate",
    {dataset: $("dataset").value, mode: $("mode").value, models: $("models").value});
  if (!ok) { $("calsummary").textContent = ""; $("calout").hidden = true;
    $("tocfg").hidden = true; $("calerr").textContent = data.error; return; }
  $("calsummary").textContent = Object.entries(data.summary)
    .map(([k, v]) => k + "=" + v).join(" · ");
  if (data.curve) {
    const max = Math.max(...data.curve.map(p => p.accuracy));
    data.curve.forEach(p => {
      const row = document.createElement("div"); row.className = "row";
      const pct = (100 * p.accuracy).toFixed(0);
      row.innerHTML = `<span class="muted" style="width:4rem">${p.threshold.toFixed(2)}</span>` +
        `<span class="track"><span class="bar" style="width:${pct}%"></span></span>` +
        `<span style="width:3rem">${p.accuracy.toFixed(2)}</span>`;
      $("curve").appendChild(row);
    });
  }
  $("calout").textContent = data.toml; $("calout").hidden = false; $("tocfg").hidden = false;
});
$("tocfg").addEventListener("click", () => {
  $("toml").value = $("calout").textContent;
  document.querySelector('nav button[data-tab="configure"]').click();
  $("cfgstatus").textContent = "pasted from calibrate — review and save";
});

// --- configure ---
async function loadConfig() {
  const r = await fetch("/api/config"); const data = await r.json();
  $("toml").value = data.toml;
}
$("validate").addEventListener("click", async () => {
  const {data} = await post("/api/config/validate", {toml: $("toml").value});
  $("cfgstatus").innerHTML = data.ok
    ? '<span class="ok">valid</span>' : '<span class="err">' + data.error + '</span>';
});
$("save").addEventListener("click", async () => {
  const {ok, data} = await post("/api/config/save", {toml: $("toml").value});
  $("cfgstatus").innerHTML = ok
    ? '<span class="ok">saved</span>' : '<span class="err">' + data.error + '</span>';
});

// --- onboard ---
let obQueue = [], obIndex = 0, obArms = [];
function parsePrompts(text) {
  return text.split("\\n").map(l => l.trim()).filter(Boolean).map(l => {
    if (l.startsWith("{")) {
      try { const o = JSON.parse(l); if (o && typeof o.text === "string") return o.text; }
      catch (e) { /* fall through to raw line */ }
    }
    return l;
  });
}
async function loadOnboardState() {
  const r = await fetch("/api/onboard"); const d = await r.json();
  obArms = d.arms;
  $("arms").textContent = d.arms.length >= 2 ? d.arms.join(" vs ")
    : "(configure two [gateway.models])";
  $("lblcount").textContent = d.count;
}
$("startob").addEventListener("click", () => {
  $("oberr").textContent = "";
  obQueue = parsePrompts($("prompts").value); obIndex = 0;
  if (!obQueue.length) { $("obprogress").textContent = "no prompts"; return; }
  if (obArms.length < 2) {
    $("oberr").textContent = "configure two [gateway.models] first"; return;
  }
  $("obcurrent").hidden = false; showCurrent();
});
async function showCurrent() {
  if (obIndex >= obQueue.length) {
    $("obcurrent").hidden = true;
    $("obprogress").textContent = "done — " + obQueue.length + " judged";
    return;
  }
  const prompt = obQueue[obIndex];
  $("obprogress").textContent = (obIndex + 1) + " / " + obQueue.length;
  $("obprompt").textContent = prompt;
  $("obarms").textContent = "running both arms…"; $("objudge").innerHTML = "";
  const {ok, data} = await post("/api/onboard/run", {prompt});
  if (!ok) { $("obarms").textContent = ""; $("oberr").textContent = data.error; return; }
  $("oberr").textContent = ""; $("obarms").innerHTML = "";
  obArms.forEach(arm => {
    const col = document.createElement("div"); col.style.flex = "1";
    const h = document.createElement("strong"); h.textContent = arm;
    const pre = document.createElement("pre"); pre.textContent = data.outputs[arm] || "";
    col.append(h, pre); $("obarms").appendChild(col);
  });
  const [primary, fallback] = obArms;
  const good = document.createElement("button");
  good.className = "act"; good.textContent = "‘" + primary + "’ good enough";
  good.onclick = () => recordJudgment(prompt, primary);
  const need = document.createElement("button");
  need.className = "act"; need.textContent = "needs ‘" + fallback + "’";
  need.onclick = () => recordJudgment(prompt, fallback);
  $("objudge").innerHTML = ""; $("objudge").append(good, need);
}
async function recordJudgment(prompt, label) {
  const {data} = await post("/api/onboard/record", {prompt, label});
  $("lblcount").textContent = data.count;
  obIndex++; showCurrent();
}
$("obcal").addEventListener("click", async () => {
  const r = await fetch("/api/onboard/dataset"); const d = await r.json();
  if (!d.dataset) { $("oberr").textContent = "no labels recorded yet"; return; }
  $("dataset").value = d.dataset;
  document.querySelector('nav button[data-tab="calibrate"]').click();
});
$("obrecal").addEventListener("click", async () => {
  $("obrecalstatus").textContent = "recalibrating…";
  const {ok, data} = await post("/api/recalibrate", {mode: "threshold"});
  if (!ok) { $("obrecalstatus").innerHTML = '<span class="err">' + data.error + '</span>'; return; }
  if (!data.written) { $("obrecalstatus").textContent = "skipped — " + data.reason; return; }
  const s = Object.entries(data.summary).map(([k, v]) => k + "=" + v).join(" · ");
  $("obrecalstatus").innerHTML = '<span class="ok">saved wayfinder-router.toml</span> · ' + s;
  loadOnboardState();
});

score();
</script>
</body>
</html>
"#;
