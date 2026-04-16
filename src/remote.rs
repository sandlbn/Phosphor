// HTTP remote control server for Phosphor.
//
// Serves a single-page web UI and REST API on a configurable port.
// Runs in a background thread — all communication with the iced App
// goes through shared state (Arc<Mutex>) and a command channel.

use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::Sender;
use serde::Serialize;

// ─────────────────────────────────────────────────────────────────────────────
//  Commands sent from HTTP server → App (polled on Tick)
// ─────────────────────────────────────────────────────────────────────────────

pub enum RemoteCmd {
    PlayTrack(usize),
    Stop,
    TogglePause,
    NextTrack,
    PrevTrack,
    SetSubtune(u16),
}

// ─────────────────────────────────────────────────────────────────────────────
//  Shared state: App → HTTP server (updated on every Tick)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Default)]
pub struct RemoteStatus {
    pub state: String,
    pub title: String,
    pub author: String,
    pub released: String,
    pub current_song: u16,
    pub songs: u16,
    pub elapsed_secs: f32,
    pub duration_secs: Option<f32>,
    pub current_index: Option<usize>,
    pub num_sids: usize,
    pub sid_type: String,
    pub is_pal: bool,
    pub engine: String,
}

#[derive(Clone, Serialize)]
pub struct RemotePlaylistEntry {
    pub index: usize,
    pub title: String,
    pub author: String,
    pub duration: Option<u32>,
    pub num_sids: usize,
    pub is_rsid: bool,
}

#[derive(Default)]
pub struct SharedRemoteState {
    pub status: RemoteStatus,
    pub playlist: Vec<RemotePlaylistEntry>,
    pub playlist_version: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Server
// ─────────────────────────────────────────────────────────────────────────────

pub fn start_server(port: u16, state: Arc<Mutex<SharedRemoteState>>, cmd_tx: Sender<RemoteCmd>) {
    thread::Builder::new()
        .name("phosphor-http".into())
        .spawn(move || {
            let addr = format!("0.0.0.0:{}", port);
            let server = match tiny_http::Server::http(&addr) {
                Ok(s) => {
                    eprintln!("[phosphor] Remote control: http://localhost:{}", port);
                    s
                }
                Err(e) => {
                    eprintln!("[phosphor] Failed to start HTTP server on {}: {e}", addr);
                    return;
                }
            };

            for request in server.incoming_requests() {
                let url = request.url().to_string();
                let method = request.method().to_string();

                match (method.as_str(), url.as_str()) {
                    // ── Web UI ───────────────────────────────────────────
                    ("GET", "/") | ("GET", "/index.html") => {
                        let resp = tiny_http::Response::from_string(WEB_UI).with_header(
                            "Content-Type: text/html; charset=utf-8"
                                .parse::<tiny_http::Header>()
                                .unwrap(),
                        );
                        let _ = request.respond(resp);
                    }

                    // ── API: status ──────────────────────────────────────
                    ("GET", "/api/status") => {
                        let json = {
                            let s = state.lock().unwrap();
                            serde_json::to_string(&s.status).unwrap_or_default()
                        };
                        respond_json(request, &json);
                    }

                    // ── API: playlist ────────────────────────────────────
                    ("GET", p) if p.starts_with("/api/playlist") => {
                        let json = {
                            let s = state.lock().unwrap();
                            serde_json::to_string(&s.playlist).unwrap_or_default()
                        };
                        respond_json(request, &json);
                    }

                    // ── API: transport controls ──────────────────────────
                    ("POST", "/api/pause") => {
                        let _ = cmd_tx.try_send(RemoteCmd::TogglePause);
                        respond_ok(request);
                    }
                    ("POST", "/api/stop") => {
                        let _ = cmd_tx.try_send(RemoteCmd::Stop);
                        respond_ok(request);
                    }
                    ("POST", "/api/next") => {
                        let _ = cmd_tx.try_send(RemoteCmd::NextTrack);
                        respond_ok(request);
                    }
                    ("POST", "/api/prev") => {
                        let _ = cmd_tx.try_send(RemoteCmd::PrevTrack);
                        respond_ok(request);
                    }

                    // ── API: play track by index ─────────────────────────
                    ("POST", p) if p.starts_with("/api/play/") => {
                        if let Some(idx_str) = p.strip_prefix("/api/play/") {
                            if let Ok(idx) = idx_str.parse::<usize>() {
                                let _ = cmd_tx.try_send(RemoteCmd::PlayTrack(idx));
                                respond_ok(request);
                            } else {
                                respond_error(request, 400, "Invalid index");
                            }
                        } else {
                            respond_error(request, 400, "Missing index");
                        }
                    }

                    // ── API: set subtune ─────────────────────────────────
                    ("POST", p) if p.starts_with("/api/subtune/") => {
                        if let Some(n_str) = p.strip_prefix("/api/subtune/") {
                            if let Ok(n) = n_str.parse::<u16>() {
                                let _ = cmd_tx.try_send(RemoteCmd::SetSubtune(n));
                                respond_ok(request);
                            } else {
                                respond_error(request, 400, "Invalid subtune");
                            }
                        } else {
                            respond_error(request, 400, "Missing subtune");
                        }
                    }

                    _ => {
                        respond_error(request, 404, "Not found");
                    }
                }
            }
        })
        .expect("Failed to spawn HTTP server thread");
}

fn respond_json(request: tiny_http::Request, json: &str) {
    let resp = tiny_http::Response::from_string(json).with_header(
        "Content-Type: application/json"
            .parse::<tiny_http::Header>()
            .unwrap(),
    );
    let _ = request.respond(resp);
}

fn respond_ok(request: tiny_http::Request) {
    respond_json(request, r#"{"ok":true}"#);
}

fn respond_error(request: tiny_http::Request, code: u16, msg: &str) {
    let json = format!(r#"{{"error":"{}"}}"#, msg);
    let resp = tiny_http::Response::from_string(json)
        .with_status_code(code)
        .with_header(
            "Content-Type: application/json"
                .parse::<tiny_http::Header>()
                .unwrap(),
        );
    let _ = request.respond(resp);
}

// ─────────────────────────────────────────────────────────────────────────────
//  Embedded Web UI
// ─────────────────────────────────────────────────────────────────────────────

const WEB_UI: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Phosphor Remote</title>
<style>
  * { margin:0; padding:0; box-sizing:border-box; }
  body { background:#0e1014; color:#c8ccd0; font-family:-apple-system,system-ui,sans-serif; }
  .header { background:#161920; padding:12px 16px; text-align:center; border-bottom:1px solid #2a2e36; }
  .header h1 { font-size:18px; color:#5cb870; letter-spacing:2px; }
  .now-playing { padding:16px; text-align:center; min-height:90px; }
  .np-title { font-size:20px; font-weight:600; color:#e0e4e8; }
  .np-author { font-size:14px; color:#8090a0; margin-top:4px; }
  .np-info { font-size:12px; color:#506070; margin-top:6px; }
  .progress { height:3px; background:#1a1e26; margin:0 16px; border-radius:2px; }
  .progress-fill { height:100%; background:linear-gradient(90deg,#3a7,#5cb870); border-radius:2px; transition:width 0.5s; }
  .controls { display:flex; justify-content:center; gap:12px; padding:16px; }
  .controls button { width:52px; height:42px; border:1px solid #2a2e36; border-radius:8px;
    background:#1a1e26; color:#c8ccd0; font-size:18px; cursor:pointer; transition:background 0.15s; }
  .controls button:hover { background:#252a34; }
  .controls button:active { background:#3a7; color:#0e1014; }
  .search { padding:8px 16px; }
  .search input { width:100%; padding:8px 12px; border:1px solid #2a2e36; border-radius:6px;
    background:#1a1e26; color:#c8ccd0; font-size:14px; outline:none; }
  .search input:focus { border-color:#3a7; }
  .playlist { overflow-y:auto; max-height:calc(100vh - 320px); }
  .track { display:flex; padding:8px 16px; border-bottom:1px solid #1a1e26; cursor:pointer;
    transition:background 0.1s; align-items:center; gap:10px; }
  .track:hover { background:#1a1e26; }
  .track.active { background:#1a2a20; border-left:3px solid #5cb870; }
  .track .idx { width:30px; font-size:11px; color:#506070; text-align:right; flex-shrink:0; }
  .track .info { flex:1; min-width:0; }
  .track .t-title { font-size:13px; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
  .track .t-author { font-size:11px; color:#607080; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
  .track .dur { font-size:11px; color:#506070; flex-shrink:0; }
  .state-dot { display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px; }
  .state-dot.playing { background:#5cb870; box-shadow:0 0 6px #5cb870; }
  .state-dot.paused { background:#c8a030; }
  .state-dot.stopped { background:#505860; }
</style>
</head>
<body>

<div class="header"><h1>PHOSPHOR</h1></div>

<div class="now-playing" id="np">
  <div class="np-title" id="np-title">—</div>
  <div class="np-author" id="np-author"></div>
  <div class="np-info" id="np-info"></div>
</div>

<div class="progress"><div class="progress-fill" id="prog" style="width:0%"></div></div>

<div class="controls">
  <button onclick="cmd('prev')" title="Previous">&#9198;</button>
  <button onclick="cmd('stop')" title="Stop">&#9209;</button>
  <button onclick="cmd('pause')" title="Play/Pause" id="pp-btn">&#9208;</button>
  <button onclick="cmd('next')" title="Next">&#9197;</button>
</div>

<div class="search"><input id="q" placeholder="Search playlist..." oninput="filterList()"></div>
<div class="playlist" id="pl"></div>

<script>
let playlist=[], filtered=[], status={}, curIdx=null;

async function cmd(c,body){
  await fetch('/api/'+c,{method:'POST'});
  setTimeout(poll,150);
}

async function poll(){
  try{
    const r=await fetch('/api/status');
    status=await r.json();
    const t=document.getElementById('np-title');
    const a=document.getElementById('np-author');
    const i=document.getElementById('np-info');
    t.textContent=status.title||'—';
    a.textContent=status.author||'';
    const elapsed=fmtTime(status.elapsed_secs||0);
    const dur=status.duration_secs?fmtTime(status.duration_secs):'--:--';
    const parts=[elapsed+' / '+dur];
    if(status.sid_type)parts.push(status.sid_type);
    if(status.is_pal!==undefined)parts.push(status.is_pal?'PAL':'NTSC');
    if(status.engine)parts.push(status.engine);
    i.innerHTML='<span class="state-dot '+status.state+'"></span>'+parts.join(' · ');
    // progress
    const pct=(status.duration_secs&&status.duration_secs>0)?
      Math.min(100,(status.elapsed_secs/status.duration_secs)*100):0;
    document.getElementById('prog').style.width=pct+'%';
    const pp=document.getElementById('pp-btn');
    pp.innerHTML=status.state==='playing'?'&#9208;':'&#9654;';
    if(status.current_index!==curIdx){curIdx=status.current_index;highlightCurrent();}
  }catch(e){}
}

function fmtTime(s){
  s=Math.floor(s);
  return Math.floor(s/60)+':'+(s%60<10?'0':'')+s%60;
}

async function loadPlaylist(){
  try{
    const r=await fetch('/api/playlist');
    playlist=await r.json();
    filterList();
  }catch(e){}
}

function filterList(){
  const q=document.getElementById('q').value.toLowerCase();
  filtered=q?playlist.filter(t=>t.title.toLowerCase().includes(q)||t.author.toLowerCase().includes(q)):playlist;
  renderList();
}

function renderList(){
  const el=document.getElementById('pl');
  el.innerHTML=filtered.map(t=>{
    const active=t.index===curIdx?'active':'';
    const dur=t.duration?fmtTime(t.duration):'';
    return '<div class="track '+active+'" onclick="cmd(\'play/'+t.index+'\')">'+
      '<span class="idx">'+(t.index+1)+'</span>'+
      '<div class="info"><div class="t-title">'+esc(t.title)+'</div>'+
      '<div class="t-author">'+esc(t.author)+'</div></div>'+
      '<span class="dur">'+dur+'</span></div>';
  }).join('');
}

function highlightCurrent(){
  document.querySelectorAll('.track').forEach(el=>{
    const idx=parseInt(el.querySelector('.idx').textContent)-1;
    el.classList.toggle('active',idx===curIdx);
  });
  // scroll active into view
  const active=document.querySelector('.track.active');
  if(active)active.scrollIntoView({block:'nearest',behavior:'smooth'});
}

function esc(s){return s?s.replace(/&/g,'&amp;').replace(/</g,'&lt;'):'';}

// Poll every second
setInterval(poll,1000);
poll();
loadPlaylist();
// Refresh playlist every 10s (in case entries change)
setInterval(loadPlaylist,10000);
</script>
</body>
</html>
"##;
