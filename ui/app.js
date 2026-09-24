// ===== 状态 =====
const S = { items: [], selected: new Set(), config: null, cdTimer: {}, cdSecs: {}, toolDl: {} };
const INVOKE = (cmd, args) => window.__TAURI__.core.invoke(cmd, args || {});
const LISTEN = window.__TAURI__.event.listen;

let toastTimer = null;
function toast(msg) {
  const t = document.getElementById('toast');
  t.textContent = msg; t.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove('show'), 2200);
}

// ===== 渲染 =====
const ST_CLS = { Probing:'probing', Ready:'ready', Downloading:'downloading', PostProcessing:'working', Transcoding:'working', Merging:'working', Done:'done', Failed:'failed', Canceled:'canceled', NeedLogin:'needlogin' };
const ST_LABEL = { Probing:'解析中', Ready:'已就绪', Downloading:'下载中', PostProcessing:'后处理中', Transcoding:'转码中', Merging:'合并中', Done:'已完成', Failed:'失败', Canceled:'已取消', NeedLogin:'需要登录' };
// 进行中的状态：后端会拒绝删除这类条目（"请先取消再删除"），兜底轮询与批量删除也按这份清单判断
const BUSY_STATUS = ['Probing','Downloading','PostProcessing','Transcoding','Merging'];

function esc(s){ return String(s==null?'':s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }

// 画质/格式列：12 项源元数据（容器·分辨率·编码·视频码率·帧率·音频编码·采样率·音频码率·音轨数·最大音量·时长·大小）
// 画质列由前端渲染（P1-6 定案：后端不产出）。
// 分辨率按**短边**口径（P0-2 依赖）：竖屏源（rotate_tag 90/270 且有 width）取 min(width,height)
function shortEdge(m){ if(!m.height)return null; const rot=[90,-90,270,-270].includes(m.rotate_tag); return rot&&m.width?Math.min(m.width,m.height):m.height; }
function resLabel(h){ return h==null?null:(h>=2160?'4K':(h>=1440?'2K':h+'P')); }
function srLabel(hz){ if(hz<1000)return hz+'Hz'; const k=hz/1000; return (Math.abs(k-Math.round(k))<0.05?String(Math.round(k)):k.toFixed(1))+'kHz'; }
function qualityLine(m){
  if(!m)return '';
  const p=[];
  if(m.container)p.push(m.container);
  const se=shortEdge(m); if(se!=null)p.push(resLabel(se));
  if(m.vcodec)p.push(m.vcodec);
  if(m.vbitrate_kbps)p.push(m.vbitrate_kbps>=1000?(m.vbitrate_kbps/1000).toFixed(1)+'Mbps':m.vbitrate_kbps+'k');
  if(m.fps)p.push(Math.round(m.fps)+'fps');
  if(m.acodec)p.push(m.acodec);
  if(m.sample_rate)p.push(srLabel(m.sample_rate));
  if(m.abitrate_kbps)p.push(m.abitrate_kbps+'k');
  if(m.audio_channels)p.push(m.audio_channels+'声道');
  if(m.audio_volume && m.audio_volume.max_volume_db!=null)p.push(m.audio_volume.max_volume_db.toFixed(1)+'dB');
  if(m.duration_secs!=null){const d=Math.floor(m.duration_secs);p.push(String(Math.floor(d/60)).padStart(2,'0')+':'+String(d%60).padStart(2,'0'));}
  if(m.size_bytes!=null)p.push(humanSize(m.size_bytes));
  return p.join(' · ');
}
function humanSize(b){
  if(b>=1073741824)return (b/1073741824).toFixed(1)+'GB';
  if(b>=1048576)return (b/1048576).toFixed(1)+'MB';
  if(b>=1024)return (b/1024).toFixed(0)+'KB';
  return b+'B';
}

function fmtMeta(it){
  const m = it.meta || {};
  let html = '<div class="meta">';
  if (it.kind === 'UrlTask' && it.status === 'Ready' && (m.download_formats||[]).length) {
    // 用 button 而不是 span：span 不可聚焦，键盘用户完全够不到"格式选择"
    html += '<button type="button" class="fmt" data-act="fmt" data-id="'+esc(it.id)+'">格式选择 ('+m.download_formats.length+')</button>';
  }
  const line = qualityLine(m);
  html += line ? esc(line) : '<span class="none">解析后将显示源元数据</span>';
  html += '</div>';
  return html;
}

function subline(it){
  if(it.url){ const s=it.site||'站点'; return esc(s+' · '+it.url); }
  if(it.path) return esc('本地文件 · '+it.path);
  return '';
}

// 支持内置登录的站点。keys 用于在条目上判断站点是否可登录；
// **须与 Rust 侧 login::login_url_for_host 保持一致**。
const LOGIN_SITES=[
  {host:'youtube.com', label:'YouTube',      keys:['youtube','youtu.be']},
  {host:'bilibili.com',label:'哔哩哔哩',      keys:['bilibili']},
  {host:'x.com',       label:'X / Twitter',  keys:['x.com','twitter']},
  {host:'douyin.com',  label:'抖音',          keys:['douyin']}
];
function canLogin(it){
  const s=((it.site||'')+' '+(it.url||'')).toLowerCase();
  return LOGIN_SITES.some(x=>x.keys.some(k=>s.includes(k)));
}

function ops(it){
  const id=esc(it.id), s=it.status;
  let h='<div class="ops">';
  const cd = (S.cdSecs[it.id] && s==='Ready' && it.kind==='UrlTask') ? '<span class="countdown">'+S.cdSecs[it.id]+'s 后自动下载</span>' : '';
  if (it.kind==='UrlTask' && s==='Ready') h += cd + '<button class="btn sm primary" data-act="dl" data-id="'+id+'">下载</button>';
  if (it.kind==='UrlTask' && s==='Ready') h += '<button class="btn sm" data-act="sec" data-id="'+id+'">剪辑</button>';
  if ((s==='Ready'||s==='Done') && it.path) h += '<button class="btn sm" data-act="tc" data-id="'+id+'">转码</button>';
  // 需要登录时必须有入口；**解析完成的 URL 任务也提供** —— 不少站点未登录
  // 同样能解析出受限清晰度（B 站只给低码率、没有 1080P+），此时状态是 Ready
  // 而不是 NeedLogin，用户就完全找不到登录入口。只对支持内置登录的站点显示，
  // 免得点了一片"该站点不支持内置登录"。
  if ((s==='NeedLogin' || (it.kind==='UrlTask' && s==='Ready')) && canLogin(it)) h += '<button class="btn sm" data-act="relogin" data-id="'+id+'">去登录</button>';
  if (s==='Downloading'||s==='PostProcessing'||s==='Transcoding'||s==='Merging') h += '<button class="btn sm danger" data-act="cancel" data-id="'+id+'">取消</button>';
  if (s==='Failed'||s==='Canceled'||s==='NeedLogin') h += '<button class="btn sm" data-act="retry" data-id="'+id+'">重试</button>';
  if (s==='Done') h += '<button class="btn sm" data-act="open" data-id="'+id+'">打开</button>';
  if (s==='Done'||s==='Failed'||s==='Canceled') h += '<button class="btn sm" data-act="del" data-id="'+id+'">删除</button>';
  // 解析/下载/后处理/转码/合并中后端会拒绝 remove_item（"请先取消再删除"）：
  // 只画一个不可点的删除示意，别让用户点了才发现删不掉
  if (BUSY_STATUS.includes(s)) h += '<button class="btn sm" disabled title="任务正在运行，请先取消再删除">删除</button>';
  h += '<button class="btn sm icon" data-act="log" data-id="'+id+'" title="日志">'+
    '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/><line x1="8" y1="13" x2="16" y2="13"/><line x1="8" y1="17" x2="16" y2="17"/></svg></button>';
  h += '</div>';
  return h;
}

function progressCell(it){
  const s=it.status;
  if (s==='Downloading'||s==='PostProcessing'||s==='Transcoding'||s==='Merging') {
    const p=Math.min(100,Math.max(0,it.percent||0));
    let sp=''; if(it.speed)sp='<span class="spd"> '+esc(it.speed)+(it.eta?' · ETA '+esc(it.eta):'')+'</span>';
    return '<div class="prog">'+p.toFixed(1)+'%'+sp+'<div class="bar"><i style="width:'+p+'%"></i></div></div>';
  }
  if (s==='Probing') return '<div class="prog"><span class="spd">解析中…</span></div>';
  if (it.error) return '<div class="cell-err" data-act="err" data-id="'+esc(it.id)+'" title="'+esc(it.error)+'">'+esc(it.error)+'</div>';
  return '<div class="prog">—</div>';
}

function rotSvg(cw){
  return cw
    ? '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-9-9"/><path d="M21 3v6h-6"/></svg>'
    : '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 9-9"/><path d="M3 3v6h6"/></svg>';
}

function rowHtml(it){
  const id=esc(it.id);
  const ra=it.rot_angle; const ang=(typeof ra==='number'?ra:((ra&&ra.degrees)||0));
  const probing = it.status==='Probing';
  // 旋转只对"文件已存在"的条目有意义（rot_angle 是转码参数）：
  // 本地文件/产物条目解析完即可设；URL 条目要等下载产物落地（后处理中起）
  const hasFile = it.kind!=='UrlTask' || ['PostProcessing','Done','Transcoding'].includes(it.status);
  const cv = probing
    ? '<div class="cv"><div class="thumb empty"><span class="ph">影</span></div></div>'
    : '<div class="cv"><div class="thumb'+(it.thumb?'':' empty')+(ang?' rot'+(ang%360):'')+'" data-id="'+id+'">'+(it.thumb?('<img src="'+esc(thumbSrc(it.thumb))+'" alt="">'):'<span class="ph">影</span>')+'</div>'+
      '<span class="rotbadge'+(ang?' show':'')+'">'+ang+'°</span></div>';
  const rots = (probing || !hasFile) ? '' :
      '<button class="rotbtn" data-act="rotcw" data-id="'+id+'" title="顺时针">'+rotSvg(true)+'</button>'+
      '<button class="rotbtn" data-act="rotccw" data-id="'+id+'" title="逆时针">'+rotSvg(false)+'</button>';
  return '<tr data-id="'+id+'">'+
    '<td><input type="checkbox" data-act="sel" data-id="'+id+'"'+(S.selected.has(it.id)?' checked':'')+'></td>'+
    '<td><div class="nrow">'+cv+
      '<div class="nmain"><div class="trow">'+rots+
        '<span class="name" title="'+esc(it.title)+'">'+esc(it.title)+'</span>'+
      '</div><div class="subline">'+subline(it)+'</div></div>'+
    '</div></td>'+
    '<td>'+fmtMeta(it)+'</td>'+
    '<td><span class="chip '+(ST_CLS[it.status]||'')+'">'+esc(ST_LABEL[it.status]||it.status)+'</span></td>'+
    '<td>'+progressCell(it)+'</td>'+
    '<td>'+ops(it)+'</td>'+
  '</tr>';
}

function render(){
  const tb=document.getElementById('tbody');
  const list=S.items.slice().sort((a,b)=>(a.updated_at||'')<(b.updated_at||'')?1:-1);
  const empty=document.getElementById('empty');
  const table=document.getElementById('mainTable');
  const wrap=document.querySelector('.table-wrap');
  const has=list.length>0;
  if(!has){empty.classList.add('show');table.style.display='none';}
  else{empty.classList.remove('show');table.style.display='';}
  // 列表卡片（白底 + 18px 圆角）只在有数据时出现，空态保持透明铺满
  if(wrap)wrap.classList.toggle('has-list',has);
  tb.innerHTML=list.map(rowHtml).join('');
  document.getElementById('selAll').checked = list.length>0 && list.every(i=>S.selected.has(i.id));
  renderBatch(); renderQueue();
}
function renderBatch(){
  const n=S.selected.size;
  document.getElementById('batchbar').classList.toggle('show',n>0);
  document.getElementById('selCount').textContent=n;
}
// 队列读数以后端 queue_status 为准：本地只能数"状态看起来在跑"的条目，
// 排队中的会被算成运行中，出现"运行中 5/3（并发上限）"这种读不懂的数字
let qstat=null;
function renderQueue(){
  const el=document.getElementById('queueInfo');
  const c=S.config&&S.config.general?S.config.general.concurrency:3;
  if(qstat){
    el.textContent='运行中 '+qstat.running+'/'+(qstat.concurrency||c)+(qstat.waiting?' · 排队 '+qstat.waiting:'');
    return;
  }
  // 后端读数拿不到（调用失败）时退回本地计数，绝不显示"未知"
  const running=S.items.filter(i=>['Downloading','PostProcessing','Transcoding','Merging'].includes(i.status)).length;
  el.textContent='运行中 '+running+'/'+c+'（并发上限）';
}
function refreshQueue(){
  INVOKE('queue_status').then(s=>{ qstat=(s&&typeof s.running==='number')?s:null; renderQueue(); })
    .catch(()=>{ qstat=null; renderQueue(); });
}
function pickLocalFiles(){
  window.__TAURI__.dialog.open({ multiple:true, directory:false,
    filters:[{name:'视频/音频',extensions:['mp4','mkv','mov','avi','flv','webm','wmv','ts','m4v','mp3','flac','wav','m4a','aac','ogg']}]
  }).then(sel=>{
    const files=Array.isArray(sel)?sel:(sel?[sel]:[]);
    if(files.length)INVOKE('add_local',{paths:files,recursive:false}).catch(err=>toast(err));
  }).catch(err=>toast(err));
}
function thumbSrc(p){
  try{ return window.__TAURI__.core.convertFileSrc(p); }catch(_){ return ''; }
}
// item:update 高频到达（yt-dlp 每条进度行都发一次），整表重渲染必须合并：
// 数据即时入 S.items，渲染按 ~200ms 合帧——否则高速下载时每秒几十次全量
// innerHTML 重建会把 UI 线程打满，看起来就像进度卡在 0% 不动
let __renderQueued=false;
function scheduleRender(){
  if(!__renderQueued){__renderQueued=true;setTimeout(()=>{__renderQueued=false;render();},200);}
}
// 进度事件每秒 5 次：整表重建会把所有 <img> 重新创建、勾选与滚动位置也一起丢，
// 所以只 patch 这一行的进度单元格与状态 chip（元素层次与 progressCell 保持一致）。
// 安全网：行或节点对不上（状态切换、速度有无变化导致结构不同）→ 退回整表 render，
// 宁可多重建一次也绝不允许"进度不动了"
function patchRow(it){
  const row=document.querySelector('tr[data-id="'+CSS.escape(it.id)+'"]');
  if(!row){scheduleRender();return;}
  const chip=row.querySelector('.chip');
  const td=row.children[4];                       // 列序：勾选/名称/画质/状态/进度/操作
  const prog=td?td.querySelector('.prog'):null;
  const pct=prog?prog.firstChild:null;            // progressCell 里 "12.3%" 是 .prog 的首个文本节点
  const spd=prog?prog.querySelector('.spd'):null; // 有速度才有这个 span
  const bar=prog?prog.querySelector('.bar > i'):null;
  // chip 的 class 与状态不一致 = 这一行已经过时（状态切换的 item:update 还没渲染），
  // 整表重画才能把操作按钮一并换成新状态该有的那一套；速度文本的有无也按 progressCell 对齐
  const hasSpd=!!spd, wantSpd=!!it.speed;
  if(!chip||!prog||!pct||pct.nodeType!==3||!bar||hasSpd!==wantSpd||chip.className!=='chip '+(ST_CLS[it.status]||'')){scheduleRender();return;}
  const p=Math.min(100,Math.max(0,it.percent||0));
  chip.textContent=ST_LABEL[it.status]||it.status;
  pct.nodeValue=p.toFixed(1)+'%';
  if(spd)spd.textContent=' '+(it.speed||'')+(it.eta?' · ETA '+it.eta:'');
  bar.style.width=p+'%';
}
function upsertItem(it){
  if(!it||!it.id)return;                 // 载荷缺失/异形时静默丢弃，别把脏数据塞进列表
  const i=S.items.findIndex(x=>x.id===it.id);
  if(i>=0)S.items[i]=it;else S.items.unshift(it);
  scheduleRender();
}

// ===== 列表对账（轻量快照）=====
// 后端 list_items_lite 不返回日志：落地时把本地已有的 log 数组接回去，
// 日志弹窗的历史内容另由 get_item_log 按需兜底
function applyList(list){
  const prev=new Map(S.items.map(i=>[i.id,i]));
  S.items=(list||[]).map(n=>{
    const old=prev.get(n.id);
    if(old&&old.log&&old.log.length)n.log=old.log;
    if(old&&old.__logCleared)n.__logCleared=true;
    return n;
  });
  // 勾选集与列表对账：clear_done / 播放列表等整批变更后，选中集合里
  // 会留下已消失的 id（"幽灵选中"），批量删除时对不存在的条目逐个报错
  const alive=new Set(S.items.map(i=>i.id));
  S.selected=new Set([...S.selected].filter(id=>alive.has(id)));
}
// 列表"结构指纹"：条目集合或状态变了才需要整表重画。进度与日志都走事件增量，
// 无条件 render 会把 <img>/勾选/滚动位置一并丢掉
function listSig(){ return S.items.map(i=>i.id+':'+i.status).join(','); }
let listSigCache='';

// ===== 确认弹窗（Promise 版）=====
// window.confirm 会阻塞 WebView2 的 JS 线程：下载中弹一下，进度事件就全停摆，
// 所以用页面已有的 modal 结构做异步确认框
let askResolve=null;
function askConfirm(msg){
  const el=document.getElementById('confirmModal');
  const txt=el?document.getElementById('confirmText'):null;
  const btnYes=el?el.querySelector('[data-confirm="1"]'):null;
  // 本机没有浏览器可验证 UI：万一弹窗节点缺失就直接退回同步 confirm，
  // 宁可回到旧行为，也不能出现"点了没反应"
  if(!el||!txt||!btnYes)return Promise.resolve(window.confirm(msg));
  txt.textContent=msg;
  // 上一个确认还没答完就再弹（如批量操作连点）：直接当取消，别让 Promise 悬挂
  if(askResolve){const r=askResolve;askResolve=null;r(false);}
  el.classList.add('show');
  btnYes.focus();
  return new Promise(res=>{ askResolve=res; });
}
function closeConfirm(ok){
  const el=document.getElementById('confirmModal');
  if(el)el.classList.remove('show');
  if(askResolve){const r=askResolve;askResolve=null;r(!!ok);}
}
const confirmEl=document.getElementById('confirmModal');
if(confirmEl)confirmEl.addEventListener('click',e=>{
  const b=e.target.closest('[data-confirm]'); if(b)closeConfirm(b.dataset.confirm==='1');
});

// ===== 事件委派（取代内联 onclick / onfocus）=====
// CSP 已是 script-src 'self'：内联事件处理器会被浏览器直接拦下，因此这些按钮
// 统一改用 data-cmd 标注，由这里派发。CMDS 的值一律写成箭头函数——被引用的
// 函数在后面才声明，箭头体在点击时才求值（避免依赖声明顺序）。
const CMDS = {
  focusUrl: ()=>document.getElementById('urlInput').focus(),
  pickLocalFiles: ()=>pickLocalFiles(),
  closeSettings: ()=>closeSettings(),
  saveSettings: ()=>saveSettings(),
  copyLogView: ()=>copyLogView(),
  clearLogView: ()=>clearLogView(),
  closeLog: ()=>closeLog(),
  closeFmt: ()=>closeFmt(),
  fmtOk: ()=>fmtOk(),
  closeUrlModal: ()=>closeUrlModal(),
  closeMerge: ()=>closeMerge(),
  closeSec: ()=>closeSec(),
  mvMerge: el=>mvMerge(Number(el.dataset.idx),Number(el.dataset.dir)),
  rmMerge: el=>rmMerge(Number(el.dataset.idx)),
};
document.addEventListener('click', e=>{
  // 点遮罩关窗（原先写在 #urlModal 的内联 handler 上）
  if(e.target&&e.target.id==='urlModal'){closeUrlModal();return;}
  const el=e.target&&e.target.closest?e.target.closest('[data-cmd]'):null;
  if(!el)return;
  const fn=CMDS[el.dataset.cmd];
  if(fn)fn(el);
});
// 内联 onfocus="this.select()" 的替代：只读输入框获得焦点即全选
document.addEventListener('focusin', e=>{
  const t=e.target;
  if(t&&t.tagName==='INPUT'&&t.readOnly)t.select();
});

// ===== 列表交互 =====
document.getElementById('tbody').addEventListener('click', e=>{
  const act=e.target.closest('[data-act]');
  if(!act)return;
  const a=act.dataset.act, id=act.dataset.id;
  if(a==='dl')startDownload(id);
  else if(a==='tc')INVOKE('start_transcode',{ids:[id]}).then(refreshQueue).catch(err=>toast(err));
  else if(a==='cancel')INVOKE('cancel_item',{id}).then(refreshQueue).catch(err=>toast(err));
  else if(a==='retry')INVOKE('retry_item',{id}).then(()=>toast('已重新解析')).catch(err=>toast(err));
  else if(a==='relogin')INVOKE('relogin_item',{id}).catch(err=>toast(err));
  else if(a==='del'){ askConfirm('删除该条目？').then(ok=>{ if(ok) INVOKE('remove_item',{id}).catch(err=>toast(err)); }); }
  else if(a==='open')INVOKE('open_item_dir',{id}).catch(err=>toast(err));
  else if(a==='log'||a==='err')openLog(id);
  else if(a==='fmt')openFmt(id);
  else if(a==='sec')openSec(id);
  else if(a==='rotcw'||a==='rotccw')rotItem(id,a==='rotcw'?90:-90);
});
document.getElementById('tbody').addEventListener('change', e=>{
  const c=e.target.closest('[data-act="sel"]');
  if(c){ if(c.checked)S.selected.add(c.dataset.id);else S.selected.delete(c.dataset.id); renderBatch(); }
});
document.getElementById('selAll').addEventListener('change', e=>{
  if(e.target.checked)S.items.forEach(i=>S.selected.add(i.id));
  else S.items.forEach(i=>S.selected.delete(i.id));
  render();
});
document.getElementById('btnAdd').addEventListener('click', addUrls);
document.getElementById('urlInput').addEventListener('keydown', e=>{ if(e.key==='Enter')addUrls(); });
document.getElementById('btnAddFile').addEventListener('click', ()=>{
  window.__TAURI__.dialog.open({ multiple:true, directory:false,
    filters:[{name:'视频/音频',extensions:['mp4','mkv','mov','avi','flv','webm','wmv','ts','m4v','mp3','flac','wav','m4a','aac','ogg']}]
  }).then(sel=>{
    const files=Array.isArray(sel)?sel:(sel?[sel]:[]);
    if(files.length)INVOKE('add_local',{paths:files,recursive:false}).catch(err=>toast(err));
  }).catch(err=>toast(err));
});
document.getElementById('btnAddDir').addEventListener('click', ()=>{
  window.__TAURI__.dialog.open({ directory:true, multiple:false })
    .then(sel=>{
      if(sel)INVOKE('add_local',{paths:[sel],recursive:true}).catch(err=>toast(err));
    }).catch(err=>toast(err));
});
document.getElementById('btnSettings').addEventListener('click', openSettings);
document.getElementById('btnClearTemp').addEventListener('click', ()=>INVOKE('clear_temp').then(()=>toast('临时文件已清理')).catch(err=>toast(err)));
// 拖放添加文件/文件夹（Tauri v2 webview onDragDropEvent）
try{
  const wv=window.__TAURI__.webview.getCurrentWebview();
  wv.onDragDropEvent(e=>{
    const t=e.payload.type;
    if(t==='enter'||t==='over'){ document.body.classList.add('drag-over'); }
    else if(t==='leave'){ document.body.classList.remove('drag-over'); }
    else if(t==='drop'){
      document.body.classList.remove('drag-over');
      const paths=e.payload.paths;
      if(paths&&paths.length){
        INVOKE('add_local',{paths,recursive:true})
          .then(()=>toast('已添加 '+paths.length+' 个路径（目录自动递归扫描）'))
          .catch(err=>toast(err));
      }
    }
  });
}catch(err){ console.warn('drag-drop unavailable:',err); }
document.getElementById('btnClearList').addEventListener('click', ()=>{
  if(!S.items.length){toast('列表已是空的');return;}
  askConfirm('清空列表（已完成/失败/已取消的条目会被移除，进行中的保留）？').then(ok=>{
    if(!ok)return;
    // 用 clear_done：后端只删终态条目，进行中的任务不会被"清"掉
    // （旧实现对每个条目发 remove_item 并吞掉错误，进行中的条目静默留下、本地列表却清空了）
    INVOKE('clear_done').then(()=>refreshQueue()).catch(err=>toast(err));
  });
});
document.getElementById('btnDelSel').addEventListener('click', ()=>{
  const ids=[...S.selected];
  // 进行中的条目后端会拒绝删除（"请先取消再删除"）：先滤掉并如实告知，
  // 而不是发出去再把错误吞掉（用户只会看到"点删了但没删掉"）
  const busy=ids.filter(id=>{const it=S.items.find(x=>x.id===id);return it&&BUSY_STATUS.includes(it.status);});
  const okIds=ids.filter(id=>!busy.includes(id));
  if(!okIds.length){toast('选中的条目都在进行中，请先取消');return;}
  askConfirm('删除选中的 '+okIds.length+' 个条目？'+(busy.length?('（'+busy.length+' 个进行中的已跳过）'):'')).then(ok=>{
    if(!ok)return;
    Promise.all(okIds.map(id=>INVOKE('remove_item',{id}).catch(err=>toast(err)))).then(()=>{S.selected.clear();render();});
  });
});
document.getElementById('btnRotCW').addEventListener('click',()=>batchRot(90));
document.getElementById('btnRotCCW').addEventListener('click',()=>batchRot(-90));
document.getElementById('btnTranscode').addEventListener('click',()=>{
  if(!S.selected.size){toast('请先勾选条目');return;}
  const ids=[...S.selected];
  INVOKE('start_transcode',{ids}).then(()=>{S.selected.clear();render();refreshQueue();}).catch(err=>toast(err));
});
document.getElementById('btnMerge').addEventListener('click',openMerge);

function addUrls(){
  const raw=document.getElementById('urlInput').value;
  const urls=raw.split(/\s+/).map(s=>s.trim()).filter(s=>/^https?:\/\//i.test(s));
  if(!urls.length){toast('请粘贴有效的视频链接');return;}
  INVOKE('add_url',{urls}).then(()=>{document.getElementById('urlInput').value='';}).catch(err=>toast(err));
}
function startDownload(id, formatId, audioOnly){
  // 行内"下载"与 5 秒倒计时自动下载都不带参数：这里补上条目已记住的选择，
  // 否则用户选过的格式/仅音频会在这一步被丢成 null（后端还会把它们写回条目）
  const it=S.items.find(i=>i.id===id);
  if(it){
    if(formatId===undefined)formatId=it.format_id;
    if(audioOnly===undefined)audioOnly=!!it.audio_only;
  }
  INVOKE('start_download',{id,formatId:formatId||null,audioOnly:audioOnly||false})
    .then(()=>{stopCountdown(id);refreshQueue();}).catch(err=>toast(err));
}
function rotItem(id,delta,silent){
  const it=S.items.find(i=>i.id===id); if(!it)return;
  const ra=it.rot_angle;
  const cur=(typeof ra==='number'?ra:((ra&&ra.degrees)||0));
  const next=((cur+delta)%360+360)%360;
  const prev=it.rot_angle;
  it.rot_angle=next;
  if(!silent)render();
  // 失败必须提示：以前静默吞掉，角度只在本机变、下次刷新又回退，用户完全看不懂。
  // 同时回滚本地乐观值，避免本地与后端长期不一致
  INVOKE('rot_item',{id,degrees:next}).catch(err=>{ it.rot_angle=prev; render(); toast(err); });
}
function batchRot(delta){
  const ids=[...S.selected];
  if(!ids.length)return;
  // silent=true：逐条 render 会把整表重建 N 次（选中 20 条就是 20 次），批量只画最终态
  ids.forEach(id=>rotItem(id,delta,true));
  render();
}

// ===== 合并面板（MG-01） =====
let M = { ids: [] };
function openMerge(){
  if(S.selected.size<2){toast('合并至少需要勾选 2 个条目');return;}
  M.ids=[...S.selected];
  document.getElementById('mergeName').value='合并_'+ts8();
  document.getElementById('mergeNorm').checked = !!(S.config&&S.config.general&&S.config.general.normalize_audio);
  renderMerge();
  document.getElementById('mergeModal').classList.add('show');
}
function closeMerge(){document.getElementById('mergeModal').classList.remove('show');}
function ts8(){const d=new Date();return ''+d.getFullYear()+('0'+(d.getMonth()+1)).slice(-2)+('0'+d.getDate()).slice(-2);}
function renderMerge(){
  document.getElementById('mergeCount').textContent=M.ids.length;
  document.getElementById('btnMergeOk').disabled=M.ids.length<2;
  const byId={};S.items.forEach(i=>byId[i.id]=i);
  document.getElementById('mergeList').innerHTML=M.ids.map((id,i)=>{
    const it=byId[id]||{title:id};
    return '<li class="merge-row"><span class="idx">'+(i+1)+'</span>'+
      '<span class="t" title="'+esc(it.title)+'">'+esc(it.title)+'</span>'+
      '<button title="上移" data-cmd="mvMerge" data-idx="'+i+'" data-dir="-1">↑</button>'+
      '<button title="下移" data-cmd="mvMerge" data-idx="'+i+'" data-dir="1">↓</button>'+
      '<button class="del" title="移除" data-cmd="rmMerge" data-idx="'+i+'">✕</button></li>';
  }).join('');
  mergeWarn();
}
function mvMerge(i,dir){
  const j=i+dir;
  if(j<0||j>=M.ids.length)return;
  const t=M.ids[i];M.ids[i]=M.ids[j];M.ids[j]=t;renderMerge();
}
function rmMerge(i){M.ids.splice(i,1);renderMerge();
  if(M.ids.length<2){document.getElementById('btnMergeOk').disabled=true;toast('合并至少需要 2 段');}
}
function mergeWarn(){
  const el=document.getElementById('mergeWarn');
  const byId={};S.items.forEach(i=>byId[i.id]=i);
  const list=M.ids.map(id=>byId[id]).filter(Boolean);
  const metas=list.map(it=>it.meta||{});
  if(metas.length<2){el.className='merge-warn warn';el.textContent='部分条目尚未解析，无法对比参数。';return;}
  // 同参判据与后端 merge.rs::same_parameters 同口径：vcodec/height/fps/acodec/sample_rate
  // 一致 且 extradata（SPS/PPS）非空并一致（未知即判不一致，MG-02）
  const v=m=>m.vcodec+'|'+m.height+'|'+(m.fps?Math.round(m.fps*100):'')+'|'+m.acodec+'|'+(m.sample_rate||'')+'|'+(m.extradata||'');
  const all=metas.every(m=>m&&m.vcodec&&m.height&&m.extradata);
  if(!all){el.className='merge-warn warn';el.textContent='缺少部分条目参数（编码/分辨率），合并将按统一模式处理。';return;}
  const first=v(metas[0]);
  const same=metas.every(m=>v(m)===first);
  if(same){el.className='merge-warn ok';el.textContent='各段编码/分辨率/帧率/音频/采样率一致 → 零重编码直拼（速度最快，无损）。';}
  else{el.className='merge-warn warn';el.textContent='检测到参数不一致（编码/分辨率/帧率/音频等）→ 将按所选编码器统一转码后拼接，耗时较长。';}
  // P1-2：手动旋转不参与合并（直拼无法逐段旋转，统一转码段也未接 rot_angle）
  const rotated=list.some(it=>{const ra=it.rot_angle;const d=typeof ra==='number'?ra:((ra&&ra.degrees)||0);return d!==0;});
  if(rotated){el.className='merge-warn warn';el.textContent+=' ⚠ 有条目带手动旋转：合并不会应用旋转，如需旋转请先对单条目转码。';}
}
document.getElementById('btnMergeOk').addEventListener('click',()=>{
  const ids=M.ids;
  const filename=document.getElementById('mergeName').value.trim()||'合并_'+ts8();
  const container=document.getElementById('mergeContainer').value;
  const encoder=document.getElementById('mergeEncoder').value;
  const normalize=document.getElementById('mergeNorm').checked;
  INVOKE('start_merge',{ids,filename,container,encoder,normalize})
    .then(()=>{M.ids=[];S.selected.clear();render();refreshQueue();closeMerge();toast('已开始合并');})
    .catch(err=>toast(err));
});

// ===== 5 秒倒计时自动下载 =====
// 每秒只 patch 倒计时节点：整表重建会重新创建所有 <img> 并丢勾选/滚动位置
function patchCountdown(id){
  const row=document.querySelector('tr[data-id="'+CSS.escape(id)+'"]');
  const el=row?row.querySelector('.countdown'):null;
  // 节点还没画出来（倒计时刚起步）→ 退回整表，宁可多画一次也不能让读数不动
  if(!el){render();return;}
  el.textContent=S.cdSecs[id]+'s 后自动下载';
}
function startCountdown(id){
  stopCountdown(id);
  let n=5; S.cdSecs[id]=n;
  const t=setInterval(()=>{
    const it=S.items.find(i=>i.id===id);
    // 条目被删，或状态已离开 Ready（手动下载/重解析/需登录）→ 停表，
    // 否则到点会对非就绪条目发起下载并弹错误
    if(!it||it.status!=='Ready'||it.kind!=='UrlTask'){clearInterval(t);delete S.cdTimer[id];delete S.cdSecs[id];render();return;}
    n--;
    if(n<=0){clearInterval(t);delete S.cdTimer[id];delete S.cdSecs[id];render();startDownload(id);return;}
    S.cdSecs[id]=n; patchCountdown(id);
  },1000);
  S.cdTimer[id]=t;
  render();
}
function stopCountdown(id){
  if(S.cdTimer[id])clearInterval(S.cdTimer[id]);
  delete S.cdTimer[id]; delete S.cdSecs[id];
  render();
}

// ===== 格式选择 =====
let fmtId=null;
function openFmt(id){
  const it=S.items.find(i=>i.id===id); if(!it)return;
  fmtId=id; stopCountdown(id);
  document.getElementById('fmtTitle').textContent='选择下载格式 - '+(it.title||'');
  const list=document.getElementById('fmtList');
  list.innerHTML=(it.meta.download_formats||[]).map(f=>
    '<li data-fid="'+esc(f.format_id)+'" data-aonly="'+(f.audio_only?'1':'0')+'">'+
    '<span>'+esc(f.label)+(f.note?' <span class="s">'+esc(f.note)+'</span>':'')+'</span>'+
    '<span class="s">'+esc(f.format_id)+'</span></li>'
  ).join('')||'<li>没有可用格式</li>';
  list.querySelectorAll('li[data-fid]').forEach(li=>{
    li.addEventListener('click',()=>{list.querySelectorAll('li').forEach(x=>x.classList.remove('on'));li.classList.add('on');});
  });
  document.getElementById('fmtModal').classList.add('show');
}
function fmtOk(){
  const on=document.querySelector('#fmtList li.on');
  if(!on){toast('请选择格式');return;}
  startDownload(fmtId,on.dataset.fid,on.dataset.aonly==='1');
  closeFmt();
}
function closeFmt(){document.getElementById('fmtModal').classList.remove('show');fmtId=null;}

// ===== 时间范围剪辑（DL-12） =====
let secId=null;
function openSec(id){
  const it=S.items.find(i=>i.id===id); if(!it)return;
  secId=id;
  document.getElementById('secStart').value=(it.sections&&it.sections[0])||'';
  document.getElementById('secEnd').value=(it.sections&&it.sections[1])||'';
  document.getElementById('secModal').classList.add('show');
}
function closeSec(){document.getElementById('secModal').classList.remove('show');secId=null;}
document.getElementById('btnSecOk').addEventListener('click',()=>{
  if(!secId)return;
  const start=document.getElementById('secStart').value.trim();
  const end=document.getElementById('secEnd').value.trim();
  INVOKE('set_sections',{id:secId,start,end})
    .then(()=>{closeSec();render();toast(start||end?'已设置时间范围':'已清除时间范围');})
    .catch(err=>toast(err));
});

// ===== 工具链接（下载/检查地址，可复制，下载慢时可手动下载） =====
function urlRowHtml(lbl,val,id){
  return '<div class="urlrow"><div class="lbl">'+lbl+'</div>'+
    '<input type="text" readonly id="'+id+'" value="'+esc(val)+'">'+
    '<button class="btn sm" data-copy="'+id+'">复制</button></div>';
}
function copyInput(id){
  const inp=document.getElementById(id);
  inp.focus();inp.select();
  const ok=()=>toast('已复制到剪贴板');
  if(navigator.clipboard&&navigator.clipboard.writeText){
    navigator.clipboard.writeText(inp.value).then(ok).catch(()=>{try{document.execCommand('copy');ok();}catch(e){toast('复制失败，请手动 Ctrl+C');}});
  }else{
    try{document.execCommand('copy');ok();}catch(e){toast('复制失败，请手动 Ctrl+C');}
  }
}
function openUrlModal(tool){
  INVOKE('tool_urls').then(list=>{
    const it=(list||[]).find(x=>x.tool===tool);
    if(!it){toast('未找到该工具的链接信息');return;}
    document.getElementById('urlTitle').textContent=it.name+' · 下载 / 检查地址';
    document.getElementById('urlBody').innerHTML=
      urlRowHtml('下载地址（「下载/更新」用的就是这个包）',it.download_url,'uDownload')+
      urlRowHtml('构建页（查最新版本 / 手动挑文件）',it.check_url,'uCheck')+
      '<div style="font-size:12px;color:var(--ink-48);line-height:1.6">手动下载后：ffmpeg/ffprobe 解压取 bin\\ 里的 exe；把 exe 路径填到依赖页输入框并保存即可，之后程序优先使用这一份。</div>';
    document.getElementById('urlModal').classList.add('show');
    document.getElementById('urlBody').querySelectorAll('[data-copy]').forEach(b=>b.addEventListener('click',()=>copyInput(b.dataset.copy)));
  }).catch(err=>toast(err));
}
function closeUrlModal(){document.getElementById('urlModal').classList.remove('show');}

// ===== 日志 =====
let logId=null;
// 单行日志渲染（openLog 全量渲染与 item:log 实时追加共用）
function logLineHtml(txtRaw){
  const txt=typeof txtRaw==='string'?txtRaw:String(txtRaw);
  let cls=''; let body=txt;
  if(txt.includes('失败')||txt.includes('错误')){cls='err';}
  if(/403|Forbidden/i.test(txt)){
    body+='\n\n[提示] HTTP 403：站点拒绝了请求。YouTube 请先点"去登录"用内置登录保存 Cookie 后重试；或更换代理节点；或更新 yt-dlp。';
    cls='err';
  }
  return '<li class="'+cls+'">'+esc(body)+'</li>';
}
function renderLog(log){
  const lines=(log&&log.length)?log:['（暂无日志）'];
  document.getElementById('logList').innerHTML=lines.map(logLineHtml).join('');
}
function scrollLogToEnd(){
  const box=document.querySelector('#logModal .log-list');
  if(box)box.scrollTop=box.scrollHeight;
}
function openLog(id){
  const it=S.items.find(i=>i.id===id); if(!it)return;
  logId=id;
  document.getElementById('logTitle').textContent='日志 - '+(it.title||id);
  // 先用本地已有内容立即出图（本地只有事件到齐之后的那些行）
  renderLog(it.log);
  document.getElementById('logModal').classList.add('show');
  scrollLogToEnd();
  // 列表走的是轻量快照（不带日志），这里按需拉一次后端权威副本。
  // 本条目被用户在弹窗内"清空"过就不再拉回来（清空是查看器的本地行为）
  if(it.__logCleared)return;
  INVOKE('get_item_log',{id}).then(lines=>{
    if(logId!==id)return;                // 期间切到别的条目了，别覆盖
    it.log=lines||[]; renderLog(it.log); scrollLogToEnd();
  }).catch(()=>{});
}
function clearLogView(){
  document.getElementById('logList').innerHTML='<li>（已清空）</li>';
  // 只清 DOM 的话数据还在 it.log 里，关闭再打开（openLog 按 it.log 全量渲染）旧日志又全回来
  const it=S.items.find(i=>i.id===logId);
  if(it){it.log=[];it.__logCleared=true;}
  toast('已清空（该条目的日志）');
}
function copyLogView(){
  const lis=document.querySelectorAll('#logList li');
  const text=Array.from(lis).map(li=>li.textContent).join('\n');
  // 与 copyInput 同口径的兜底：无剪贴板 API 时直接说清楚，不要抛未捕获的 TypeError
  if(!(navigator.clipboard&&navigator.clipboard.writeText)){toast('当前环境不支持剪贴板，请手动选择复制');return;}
  navigator.clipboard.writeText(text).then(()=>toast('日志已复制到剪贴板')).catch(()=>toast('复制失败'));
}
function closeLog(){document.getElementById('logModal').classList.remove('show');logId=null;}

// ===== 设置 =====
let curPage='deps';
function openSettings(){
  INVOKE('get_config').then(c=>{S.config=c;S.configSaved=JSON.stringify(c);renderSetPage();document.getElementById('settingsModal').classList.add('show');}).catch(err=>toast(err));
}
function closeSettings(){
  // 防呆：改了没保存就关闭（分流站点/代理不生效的最常见原因），确认后再丢
  const changed=S.configSaved!=null && JSON.stringify(S.config)!==S.configSaved;
  if(!changed){document.getElementById('settingsModal').classList.remove('show');return;}
  askConfirm('设置有未保存的修改，确定关闭（修改将丢失）？').then(ok=>{
    if(ok)document.getElementById('settingsModal').classList.remove('show');
  });
}
document.getElementById('setNav').addEventListener('click', e=>{
  const b=e.target.closest('button[data-page]'); if(!b)return;
  curPage=b.dataset.page;
  document.querySelectorAll('#setNav button').forEach(x=>x.classList.toggle('on',x===b));
  renderSetPage();
});
function cfgRef(){return S.config||{download:{},transcode:{},general:{},dependencies:{},network:{}};}

function probeHw(){
  // 探测失败也要回填一个"全不支持"的结论并重画：原来只吞掉错误，
  // 编码器选项旁的"（未检测到）"标注永远不出现
  INVOKE('probe_hw_encoders')
    .then(r=>{S.hw=r;})
    .catch(()=>{S.hw={qsv:false,nvenc:false,amf:false};})
    .then(()=>{ if(curPage==='transcode')renderSetPage(); });
}

function renderSetPage(){
  const c=cfgRef();
  const d=c.download||{},t=c.transcode||{},g=c.general||{},dp=c.dependencies||{},n=c.network||{};
  const page=document.getElementById('setPage');
  const row=(k,inner,small)=>'<div class="set-row"><div class="k">'+k+(small?'<small>'+small+'</small>':'')+'</div><div class="v grow">'+inner+'</div></div>';
  const txt=(path,val,ph)=>'<input type="text" data-cfg="'+path+'" value="'+esc(val==null?'':val)+'" placeholder="'+esc(ph||'')+'">';
  const num=(path,val,min,max)=>'<input type="number" data-cfg="'+path+'" value="'+esc(val==null?'':val)+'"'+(min!=null?' min="'+min+'"':'')+(max!=null?' max="'+max+'"':'')+'>';
  const sel=(path,val,opts)=>'<select data-cfg="'+path+'">'+opts.map(o=>'<option value="'+o[0]+'"'+(String(val)===String(o[0])?' selected':'')+'>'+o[1]+'</option>').join('')+'</select>';
  const chk=(path,val,label)=>'<label class="chk2"><input type="checkbox" data-cfg="'+path+'"'+(val?' checked':'')+'>'+label+'</label>';
  // 「下载中」状态存在 S.toolDl 而不是 DOM：设置页换子页/返回都会重建 DOM，
  // 状态只放 DOM 时按钮会变回"下载"，进度与取消入口一起消失（下载其实还在跑）
  const toolRow=(key,label)=>{
    const dl=S.toolDl&&S.toolDl[key];
    const dlBtn=dl
      ? '<button class="btn sm danger" data-act="tool-cancel" data-tool="'+key+'">取消 '+(dl.phase||'下载')+' '+(dl.percent||0)+'%</button>'
      : '<button class="btn sm" data-act="tool-dl" data-tool="'+key+'">下载</button>';
    return '<div class="set-row"><div class="k">'+label+'</div><div class="v grow"><div class="toolrow">'+
      txt('dependencies.'+key,dp[key],'留空 = 系统 PATH')+
      dlBtn+
      '<button class="btn sm" data-act="tool-up" data-tool="'+key+'"'+(dl?' disabled':'')+'>更新</button>'+
      '<button class="btn sm" data-act="tool-urls" data-tool="'+key+'" title="查看/复制下载地址，下载慢可手动下载">链接</button></div></div></div>';
  };

  let h='';
  if(curPage==='deps'){
    h+='<div class="set-hd">工具链（查找顺序：设置路径 → tools\\ → 系统 PATH；「下载」装到 tools\\ 并记住路径，「更新」只更新当前生效的那一份；下载慢可点「链接」复制地址手动下载）</div>';
    h+=toolRow('yt_dlp_path','yt-dlp 路径');
    h+=toolRow('ffmpeg_path','ffmpeg 路径');
    h+=toolRow('ffprobe_path','ffprobe 路径');
    h+=toolRow('deno_path','deno 路径');
    h+=row('PO-Token 服务',chk('dependencies.potoken_enabled',dp.potoken_enabled,'启用').replace('<input ','<input disabled '),'【暂未生效】PO-Token（POT）是 YouTube 风控验证令牌（Proof-of-Origin Token），由 deno 运行 potoken 生成器产出，用于通过 YouTube 机器人验证、降低 403/风控拦截概率。默认启用。');
  } else if(curPage==='network'){
    h+=row('代理地址',txt('network.proxy_url',n.proxy_url,'socks5://127.0.0.1:10808')+
      '<div class="proto-tags"><span class="proto-hint">常用：</span>'+
      '<button class="proto-tag" data-proxy-fill="http://127.0.0.1:7890" title="Clash 混合端口（HTTP/SOCKS5 通用）">Clash · 7890</button>'+
      '<button class="proto-tag" data-proxy-fill="socks5://127.0.0.1:10808" title="V2RayN 默认 SOCKS 端口">V2RayN · 10808</button>'+
      '<button class="proto-tag" data-proxy-fill="socks5://127.0.0.1:1080" title="Shadowsocks 默认 SOCKS 端口">SS · 1080</button>'+
      '</div>','仅对站点分流中勾选的站点生效；留空 = 全部直连（yt-dlp 可能被系统代理接管）');
    const smap=n.site_proxy||{};
    h+='<div class="set-hd">站点分流（勾选 = 走代理，其余直连）</div><div class="set-row"><div class="v grow">'+
      '<div class="'+(Object.keys(smap).length?'site-list':'site-empty')+'" id="siteList">'+renderSiteList(smap)+'</div>'+
      '<button class="site-add" id="btnSiteAdd">+ 添加站点</button></div></div>';
  } else if(curPage==='cookie'){
    h+='<div class="set-hd">内置登录（点站点打开登录窗；登录后点窗内"登录完成"保存 Cookie）</div>'+
      '<div class="set-row"><div class="v grow"><div class="btn-wrap">'+
      LOGIN_SITES.map(x=>'<button class="btn sm" data-login-host="'+x.host+'">'+x.label+'</button>').join('')+
      '<button class="btn sm" id="btnCustomLogin">自定义…</button>'+
      '</div></div></div>'+
      '<div class="set-hd">站点 Cookie（存储于 &lt;exe 同级&gt;\\config\\cookies\\，按 HOST 分文件，含 HttpOnly）</div>'+
      '<div class="set-row"><div class="v grow"><div class="site-list" id="cookieList"><div class="site-loading">加载中…</div></div></div></div>'+
      '<div class="set-row"><div class="k">说明<small>未登录也能解析的站点（如 B 站低码率）用上面的内置登录换高清晰度</small></div><div class="v"><button class="btn sm" data-act="refresh-cookie">刷新</button></div></div>';
  } else if(curPage==='download'){
    h+=row('画质上限（短边，超限自动降分辨率转码）',num('download.max_h',d.max_h,0,4320));
    h+=row('下载高度硬上限',num('download.max_dl_h',d.max_dl_h,0,4320).replace('<input ','<input disabled '),'【暂未生效】该配置项后端尚未读取，改动不生效。');
    h+=row('并发分片数',num('download.fragments',d.fragments,1,16));
    h+=row('重试次数',num('download.retries',d.retries,0,10));
    h+=row('仅音频默认',chk('download.audio_only',d.audio_only,''));
    h+=row('播放列表默认',chk('download.playlist',d.playlist,''));
    h+=row('嵌入封面 / 元数据',chk('download.embed_cover',d.embed_cover,''));
    h+=row('文件名模板（下载/转码/合并输出共用）',sel('download.filename_template',d.filename_template,[['纯标题','纯标题'],['标题+ID','标题+ID'],['UP主-标题','UP主-标题'],['日期-标题','日期-标题']]));
  } else if(curPage==='transcode'){
    if(S.hw===undefined)probeHw();
    h+=row('长边上限 MAXW',num('transcode.max_w',t.max_w,0,7680));
    h+=row('短边上限 MAXH',num('transcode.max_h',t.max_h,0,4320));
    // 后端是 Option<u32>：用文本框会把 "5000" 当字符串发过去，serde 类型不匹配会让整份设置保存失败
    h+=row('码率封顶 kbps（留空 = 自动）',num('transcode.brcap_kbps',t.brcap_kbps,1,200000));
    h+=row('兜底码率 kbps',num('transcode.br_default_kbps',t.br_default_kbps,0,100000));
    const encOpts=[['auto','自动'],['libx265','libx265'],['nvenc','NVENC H.265'+(S.hw&&!S.hw.nvenc?'（未检测到）':'')],['amf','AMF H.265'+(S.hw&&!S.hw.amf?'（未检测到）':'')]];
    h+=row('默认编码器（自动 = QSV → libx265 兜底）',sel('transcode.force_encoder_mode',t.force_encoder_mode,encOpts),'硬件编码器运行失败自动回退 libx265；未检测到的选项仍可选用');
    h+=row('QSV low_power',chk('transcode.low_power',t.low_power,''));
    h+=row('保留封面',chk('transcode.keep_cover',t.keep_cover,''));
  } else if(curPage==='general'){
    h+=row('默认输出目录（留空 = 桌面）',txt('general.default_output_dir',g.default_output_dir,'桌面'));
    h+=row('碰撞命名策略',sel('general.collision_policy',g.collision_policy,[['auto_inc','自动 +1 递增'],['skip','SKIP 跳过']]));
    h+=row('音量归一化（下载后处理与转码共用）',chk('general.normalize_audio',g.normalize_audio,''),'按解析音量增益至峰值 0dBFS，接近满度不处理');
    h+=row('音量增益上限 dB',num('general.max_gain_db',g.max_gain_db,0,48));
    h+=row('并发任务数（全局：下载/转码/合并共享）',num('general.concurrency',g.concurrency,1,16));
    h+=row('启动时检查更新',chk('general.check_update',g.check_update,'').replace('<input ','<input disabled '),'【暂未生效】该配置项后端尚未读取，改动不生效。');
    h+=row('历史上限（条，默认 100、上限 200）',num('general.history_limit',g.history_limit,1,200),'超出上限时优先裁剪最旧的终态条目（进行中的任务不会被裁掉）。');
    h+=row('清理解析缓存','<button class="btn sm" data-act="clear-cache">清理解析缓存</button>','解析缓存存于 config/cache.json + config/cache/（P1 接入）');
  }
  page.innerHTML=h;
  bindSetEvents();
  if(curPage==='cookie')loadCookieList();
}

function renderSiteList(map){
  const keys=Object.keys(map||{});
  if(!keys.length)return '<div class="site-none">尚未配置站点分流<small>点击下方"添加站点"输入 HOST（如 bilibili.com），勾选 = 走代理</small></div>';
  return keys.map(k=>'<div class="site-row"><span class="host">'+esc(k)+'</span>'+
    '<label class="chk2"><input type="checkbox" data-site="'+esc(k)+'"'+(map[k]?' checked':'')+'>走代理</label>'+
    '<button class="del" data-del-site="'+esc(k)+'">×</button></div>').join('');
}
function loadCookieList(){
  INVOKE('list_cookies').then(list=>{
    const el=document.getElementById('cookieList'); if(!el)return;
    if(!list.length){el.className='site-empty';el.innerHTML='<div class="site-none">暂无已保存的站点 Cookie<small>去登录站点 → 顶部"登录完成"按钮即可保存 Cookie</small></div>';return;}
    el.className='site-list';
    el.innerHTML=list.map(c=>'<div class="site-row"><span class="host">'+esc(c.host)+'</span>'+
      '<span class="meta">'+c.count+' 条</span>'+
      '<button class="del" data-del-cookie="'+esc(c.host)+'">×</button></div>').join('');
  }).catch(err=>{
    // 静默吞掉的话面板会永远停在"加载中…"，看不出到底是没 Cookie 还是调用失败
    const el=document.getElementById('cookieList');
    if(el)el.innerHTML='<div class="site-loading">加载失败：'+esc(err)+'</div>';
    toast(err);
  });
}
function bindSetEvents(){
  const page=document.getElementById('setPage');
  page.querySelectorAll('[data-cfg]').forEach(el=>{
    // text/number 用 input（实时写回，避免填完直接点保存没失焦丢值）；
    // checkbox/select 用 change（勾选/选中即触发）
    const evt=(el.type==='checkbox'||el.tagName==='SELECT')?'change':'input';
    el.addEventListener(evt,()=>{
      const path=el.dataset.cfg;
      let val;
      if(el.type==='checkbox')val=el.checked;
      else if(el.type==='number')val=el.value===''?null:Number(el.value);
      else val=el.value;
      setCfg(path,val);
    });
  });
  // 常用代理端口快捷标签：填入输入框并同步进配置（点保存按钮统一落盘）
  page.querySelectorAll('[data-proxy-fill]').forEach(b=>b.addEventListener('click',()=>{
    const inp=page.querySelector('input[data-cfg="network.proxy_url"]');
    if(!inp)return;
    inp.value=b.dataset.proxyFill;
    setCfg('network.proxy_url',inp.value);
    toast('已填入 '+b.textContent.trim()+'，点"保存"生效');
  }));
  page.querySelectorAll('[data-del-site]').forEach(b=>b.addEventListener('click',()=>{
    delete cfgRef().network.site_proxy[b.dataset.delSite]; renderSetPage();
  }));
  // cookie 删除用事件委托（列表是异步加载的，直接绑定时元素还不存在）
  const cookieListEl=page.querySelector('#cookieList');
  if(cookieListEl)cookieListEl.addEventListener('click',e=>{
    const b=e.target.closest('[data-del-cookie]'); if(!b)return;
    INVOKE('delete_cookie',{host:b.dataset.delCookie}).then(()=>loadCookieList()).catch(err=>toast(err));
  });
  page.querySelectorAll('[data-act="tool-dl"],[data-act="tool-up"],[data-act="tool-cancel"]').forEach(b=>b.addEventListener('click',()=>{
    const tool=b.dataset.tool;
    // 已是取消按钮 → 取消下载
    if(b.dataset.act==='tool-cancel'){
      INVOKE('cancel_tool_download',{tool}).then(()=>toast('已请求取消下载')).catch(err=>toast(err));
      return;
    }
    const isUpdate=b.dataset.act==='tool-up';
    const row=b.closest('.toolrow');
    row.querySelectorAll('.btn:not([data-act="tool-urls"])').forEach(x=>{x.disabled=true;});
    b.disabled=false;
    b.dataset.act='tool-cancel';
    b.classList.add('danger');
    b.textContent='取消';
    S.toolDl[tool]={phase:'连接',percent:0};
    INVOKE('download_tool',{tool,update:isUpdate})
      .then(r=>{
        // 「下载」= 装到 tools\ 并把该路径写回设置（之后优先用这份）；「更新」只换文件内容，不改路径
        if(!isUpdate){
          S.config=S.config||{}; S.config.dependencies=S.config.dependencies||{};
          S.config.dependencies[tool]=r.path;
          // 落盘失败不能吞：界面提示"完成"、重启后路径却没了（工具静默回退 PATH）
          INVOKE('save_config',{config:S.config})
            .catch(err=>toast('工具已装好，但路径未保存（'+err+'）：重启后仍按 PATH 查找'));
        }
        toast(r.message||('完成：'+r.path));
        delete S.toolDl[tool];
        renderSetPage();refreshDeps();
      })
      .catch(err=>{toast(err);delete S.toolDl[tool];renderSetPage();refreshDeps();});
  }));
  page.querySelectorAll('[data-act="tool-urls"]').forEach(b=>b.addEventListener('click',()=>openUrlModal(b.dataset.tool)));
  page.querySelectorAll('[data-site]').forEach(b=>b.addEventListener('change',()=>{
    const map=cfgRef().network.site_proxy||{}; map[b.dataset.site]=b.checked; cfgRef().network.site_proxy=map;
  }));
  const addSite=document.getElementById('btnSiteAdd');
  if(addSite)addSite.addEventListener('click',()=>{
    let h=prompt('输入站点（如 bilibili.com）'); if(!h)return;
    // 自动从完整 URL 提取 host（去掉 scheme、www、路径）
    h=h.trim().replace(/^https?:\/\//i,'').replace(/^www\./,'').replace(/[\/?#].*$/,'').trim();
    if(!h)return;
    const map=cfgRef().network.site_proxy||{}; map[h]=true; cfgRef().network.site_proxy=map; renderSetPage();
  });
  const refresh=page.querySelector('[data-act="refresh-cookie"]');
  if(refresh)refresh.addEventListener('click',loadCookieList);
  page.querySelectorAll('[data-login-host]').forEach(b=>b.addEventListener('click',()=>{
    INVOKE('open_login_site',{host:b.dataset.loginHost}).catch(err=>toast(err));
  }));
  const customLogin=page.querySelector('#btnCustomLogin');
  if(customLogin)customLogin.addEventListener('click',()=>{
    const url=prompt('输入要登录的站点 URL（如 https://www.bilibili.com）：');
    if(!url)return;
    try{
      const u=new URL(url);
      INVOKE('open_login_site',{host:u.hostname}).catch(err=>toast(err));
    }catch(e){toast('URL 格式不正确');}
  });
  const clearCache=page.querySelector('[data-act="clear-cache"]');
  if(clearCache)clearCache.addEventListener('click',()=>toast('解析缓存清除（P1 接入）'));
}
function setCfg(path,val){
  const parts=path.split('.');
  let obj=cfgRef();
  for(let i=0;i<parts.length-1;i++){ if(!obj[parts[i]])obj[parts[i]]={}; obj=obj[parts[i]]; }
  obj[parts[parts.length-1]]=val;
}
function refreshDeps(){
  INVOKE('probe_dependencies').then(list=>{
    const map={yt_dlp:'depYt','yt-dlp':'depYt',ffmpeg:'depFfmpeg',ffprobe:'depFfprobe',deno:'depDeno'};
    const elById={depYt:document.getElementById('depYt'),depFfmpeg:document.getElementById('depFfmpeg'),depFfprobe:document.getElementById('depFfprobe'),depDeno:document.getElementById('depDeno')};
    list.forEach(t=>{
      const el=elById[map[t.tool]]; if(!el)return;
      el.className='dep '+(t.ok?'ok':'bad');
      // 已解析到路径但版本探测失败时不能报"未找到"，否则会让人误以为工具不存在
      el.textContent=t.tool+(t.ok?(t.version?(' '+t.version):'（已找到）'):'（未找到）');
      el.title=t.path||'tools\\ 与系统 PATH 中均未找到';
    });
  }).catch(err=>{
    toast(err);
    // 探测失败时不能保留上一次的绿点：留着"假绿"会让人以为工具都还在
    const labels={depYt:'yt-dlp',depFfmpeg:'ffmpeg',depFfprobe:'ffprobe',depDeno:'deno'};
    Object.keys(labels).forEach(k=>{
      const el=document.getElementById(k); if(!el)return;
      el.className='dep bad'; el.textContent=labels[k]+' 探测失败'; el.title=String(err);
    });
  });
}
function saveSettings(){
  INVOKE('save_config',{config:S.config}).then(()=>{toast('设置已保存');closeSettings();}).catch(err=>toast(err));
}

// ===== 初始化 =====
async function init(){
  try{
    // 监听必须先于拉数据注册：首个 await（list_items）一旦失败就跳到末尾 catch，
    // 那时如果还没监听，所有事件全丢，界面表现为"点什么都没反应"
    const unlisteners=[];
    LISTEN('item:update', e=>{ upsertItem(e.payload); }).then(un=>unlisteners.push(un)).catch(()=>{});
    // 高频进度：只增量改字段（payload 仅含变化项），不接收全量条目
    LISTEN('item:progress', e=>{
      const p=e.payload; if(!p||!p.id)return;
      const it=S.items.find(x=>x.id===p.id);
      if(!it)return;
      if(p.percent!=null)it.percent=p.percent;
      if(p.speed!=null)it.speed=p.speed;
      if(p.eta!=null)it.eta=p.eta;
      if(p.file!=null)it.file=p.file;
      patchRow(it);
    }).then(un=>unlisteners.push(un)).catch(()=>{});
    // 高频日志：只带 id + 单行，前端增量 append（不触发列表重渲染）
    LISTEN('item:log', e=>{
      const d=e.payload; if(!d||!d.id)return;
      const it=S.items.find(x=>x.id===d.id);
      if(it){
        if(!it.log)it.log=[];
        it.log.push(d.line);
        if(it.log.length>300)it.log.shift();
      }
      // 日志弹窗正打开该条目：实时追加并跟随滚动
      if(logId===d.id && document.getElementById('logModal').classList.contains('show')){
        const list=document.getElementById('logList');
        // 首次实时追加前清掉占位行
        const placeholder=list.querySelector('li');
        if(placeholder && placeholder.textContent==='（暂无日志）')list.innerHTML='';
        list.insertAdjacentHTML('beforeend', logLineHtml(d.line));
        // DOM 同样裁到 300 行（与 it.log 上限一致），否则长任务会把弹窗堆到几千行
        while(list.children.length>300)list.removeChild(list.firstChild);
        const box=document.querySelector('#logModal .log-list');
        if(box)box.scrollTop=box.scrollHeight;
      }
    }).then(un=>unlisteners.push(un)).catch(()=>{});
    // 后端已改为对象载荷 { id }（旧格式是裸字符串 id，一并兼容）
    LISTEN('item:removed', e=>{
      const rid=(e.payload&&e.payload.id)||e.payload;
      S.items=S.items.filter(i=>i.id!==rid); S.selected.delete(rid); stopCountdown(rid); render();
    }).then(un=>unlisteners.push(un)).catch(()=>{});
    LISTEN('list:changed', async()=>{
      // 回调体内必须自带 try/catch：挂在外层 listen() 上的 catch 管不到这里，
      // list_items 一旦 reject，render 永不执行 —— 列表会永远停在旧数据
      try{ applyList(await INVOKE('list_items_lite')); listSigCache=listSig(); render(); }
      catch(err){ toast('列表刷新失败：'+err); }
    }).then(un=>unlisteners.push(un)).catch(()=>{});
    LISTEN('item:ready', e=>{
      const it=S.items.find(i=>i.id===e.payload.id);
      if(it&&it.kind==='UrlTask'&&it.status==='Ready')startCountdown(it.id);
    }).then(un=>unlisteners.push(un)).catch(()=>{});
    LISTEN('login:done', ()=>{
      toast('Cookie 已保存，正在重新解析需要登录的条目');
      // 真正触发重解析：NeedLogin 条目逐个 retry（后端 retry_item 允许该状态）
      S.items.filter(i=>i.status==='NeedLogin')
        .forEach(i=>INVOKE('retry_item',{id:i.id}).catch(()=>{}));
    }).then(un=>unlisteners.push(un)).catch(()=>{});
    LISTEN('cookies:changed', ()=>{ if(curPage==='cookie')loadCookieList(); }).then(un=>unlisteners.push(un)).catch(()=>{});
    LISTEN('tool:progress', e=>{
      const p=e.payload; if(!p||!p.tool)return;
      const pct=Math.max(0,Math.min(100,Math.round((p.percent||0)*100)));
      const phase=p.phase||'下载';
      // 状态先落到 JS：设置页切子页再回来会重建 DOM，按钮是新的，
      // 只有从 state 回填才能继续显示"取消 + 进度"
      S.toolDl[p.tool]={phase,percent:pct};
      const cancelBtn=document.querySelector('.toolrow [data-tool="'+CSS.escape(p.tool)+'"][data-act="tool-cancel"]');
      if(!cancelBtn)return;
      cancelBtn.textContent='取消 '+phase+' '+pct+'%';
      cancelBtn.title=phase+' '+pct+'%';
    }).then(un=>unlisteners.push(un)).catch(()=>{});
    // 关窗/刷新时解除监听，避免 WebView 重载后旧监听器堆积
    window.addEventListener('beforeunload', ()=>{ unlisteners.forEach(fn=>{ try{ fn(); }catch(_){} }); });
    // 队列读数不等 list_items：拉数据失败也要能显示（内部失败会退回本地计数）
    refreshQueue();
    // 兜底轮询：每 1.5s 同步一次队列读数（queue_status），活动任务期间再对账列表。
    // 用**轻量快照**（不带日志，单次 IPC 从 MB 级降到几十 KB），且只在
    // "条目集合/状态"真的变了才整表重画 —— 进度与日志已由事件增量维护，
    // 无条件 render 会把 <img>/勾选/滚动位置一并丢掉
    setInterval(()=>{
      const active=S.items.some(i=>BUSY_STATUS.includes(i.status));
      refreshQueue();
      if(!active)return;
      INVOKE('list_items_lite').then(list=>{
        applyList(list);
        const sig=listSig();
        if(sig!==listSigCache){listSigCache=sig;render();}
      }).catch(()=>{});
    },1500);
    // 首屏用完整列表（日志弹窗需要已有历史），之后一律走轻量快照
    applyList(await INVOKE('list_items'));
    listSigCache=listSig();
    try{ S.config=await INVOKE('get_config'); }catch(_){}
    render();
    refreshDeps();
  }catch(err){ console.error(err); toast('初始化失败：'+err); }
}
init();
