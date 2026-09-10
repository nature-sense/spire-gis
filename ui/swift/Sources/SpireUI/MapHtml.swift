import Foundation

/// The map page loaded into the WKWebView: MapLibre GL JS (from CDN) with a
/// light raster base map and one GeoJSON source per imported layer (each
/// feature carries `properties.id`, the node uuid — the single feature id
/// space used by click-to-select, detail lookup, and query matching).
///
/// Base layers render with plain paints. Selection and query results are drawn
/// by TEMPORARY companion layers added on top of the same sources (same
/// class/layer colours, matched-feature id filters); clearing a query removes
/// those layers and never disturbs the user's layer toggles. (Feature-state
/// paints are deliberately avoided: MapLibre 4.7.1 in this WKWebView throws
/// when a data-driven-painted layer is flipped visible via setLayoutProperty.)
///
/// JS → Swift messages are posted to `window.webkit.messageHandlers.spireBridge`
/// (`ready`, `tile`, `bounds`, `log`); Swift → JS uses `window.spire*` fns.
let spireMapHtml = #"""
<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<link href="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.css" rel="stylesheet">
<script src="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.js"></script>
<style>html,body,#m{margin:0;height:100%;width:100%;overflow:hidden}
#legend{position:absolute;bottom:12px;left:12px;background:rgba(255,255,255,.88);border:1px solid #ddd;border-radius:8px;padding:8px 10px;font:12px -apple-system,sans-serif;color:#333;z-index:10;box-shadow:0 1px 4px rgba(0,0,0,.18)}
#legend .lg-title{font-weight:600;margin-bottom:3px}
#legend .lg-row{display:flex;align-items:center;gap:7px;padding:1px 0}
#legend .lg-swatch{width:20px;height:5px;border-radius:3px;display:inline-block}
#legend .lg-swatch.sq{width:12px;height:12px;border-radius:2px;border:1px solid rgba(0,0,0,.2)}
</style></head>
<body><div id="m"></div><div id="legend" style="display:none"></div>
<script>
const post = (o)=>window.webkit && window.webkit.messageHandlers &&
  window.webkit.messageHandlers.spireBridge && window.webkit.messageHandlers.spireBridge.postMessage(o);
const PALETTE = ['#e6194b','#3cb44b','#4363d8','#f58231','#911eb4','#42d4f4','#f032e6'];
// Line classification → paint + legend (single source of truth for both).
const LINE_STYLES = [
  {key:'Layers/Contour_250K',        label:'Contours',     color:'#b39b7d', width:1.5, opacity:0.85},
  {key:'Layers/Major_Road',          label:'Major roads',  color:'#e63946', width:2.5, opacity:1},
  {key:'Layers/Expressway',          label:'Expressway',   color:'#2563eb', width:3.5, opacity:1},
  {key:'Layers/Expressway_Sliproad', label:'Sliproads',    color:'#f4a261', width:2,   opacity:1},
  {key:'Layers/International_bdy',   label:'Boundary',     color:'#264653', width:2,   opacity:1},
];
function linePaint(){
  const color=['match',['get','class']];
  const width=['match',['get','class']];
  const opac =['match',['get','class']];
  LINE_STYLES.forEach(s=>{ color.push(s.key,s.color); width.push(s.key,s.width); opac.push(s.key,s.opacity); });
  color.push('#a5a5a5'); width.push(1.5); opac.push(1);
  return {type:'line',paint:{'line-color':color,'line-width':width,'line-opacity':opac}};
}
// Area classification → fill paint + legend (shared table).
const POLY_STYLES = [
  {key:'Layers/Hydrographic',              label:'Water',                 color:'#4aa3df', opacity:0.55},
  {key:'Layers/Coastal_Outlines',          label:'Coast / shoreline',     color:'#b3a98c', opacity:0.6},
  {key:'Layers/Parks_NaturalReserve',      label:'Parks & reserves',      color:'#7bbf6a', opacity:0.6},
  {key:'Layers/Airport_Runway',            label:'Airport / runway',      color:'#9aa0a6', opacity:0.7},
  {key:'Layers/Central_Business_District', label:'Central business dist.',color:'#f2c94c', opacity:0.7},
];
function polygonPaint(){
  const color=['match',['get','class']];
  const opac =['match',['get','class']];
  POLY_STYLES.forEach(s=>{ color.push(s.key,s.color); opac.push(s.key,s.opacity); });
  // Inset annotations / anything unknown → light warm gray.
  color.push('#e6d8a8'); opac.push(0.5);
  return {type:'fill',paint:{'fill-color':color,'fill-opacity':opac,
    'fill-outline-color':['match',['get','class'],'Layers/Coastal_Outlines','#5fa8d3','#8a8a8a']}};
}
function legendGroup(title, styles, square){
  const g=document.createElement('div');
  const t=document.createElement('div'); t.className='lg-title'; t.textContent=title;
  g.appendChild(t);
  styles.forEach(s=>{
    const row=document.createElement('div'); row.className='lg-row';
    const sw=document.createElement('span');
    sw.className = square ? 'lg-swatch sq' : 'lg-swatch';
    sw.style.background=s.color; sw.style.opacity=s.opacity;
    const lb=document.createElement('span'); lb.textContent=s.label;
    row.appendChild(sw); row.appendChild(lb); g.appendChild(row);
  });
  return g;
}
function renderLegend(showLines, showAreas){
  const el=document.getElementById('legend'); if(!el) return;
  el.innerHTML='';
  if(showLines) el.appendChild(legendGroup('Lines', LINE_STYLES, false));
  if(showAreas) el.appendChild(legendGroup('Areas', POLY_STYLES, true));
  el.style.display=(showLines||showAreas)?'block':'none';
}
const style = {
  version:8,
  sources:{
    base:{type:'raster',tileSize:256,
      tiles:['https://www.onemap.gov.sg/maps/tiles/GreyLite/{z}/{x}/{y}.png'],
      minzoom:0,maxzoom:19,
      attribution:'&copy; OneMap | Singapore Land Authority'}
  },
  layers:[
    {id:'base',type:'raster',source:'base'}
  ]
};
const map = new maplibregl.Map({container:'m',style:style,center:[103.85,1.35],zoom:11});
window.map = map;
// Clamp the camera to OneMap/Singapore coverage so the map never pans into
// blank tiles or zooms out to oversized overview labels.
map.setMaxBounds([[103.5,1.1],[104.5,1.6]]);
map.setMinZoom(9);
map.setMaxZoom(19);
const selectableLayers=[];
// Selection + query matches render as TEMPORARY layers on top of the base
// layers (same sources, same colours) — never via feature-state paints.
let selLayers=[];   // active click-selection layer ids
let hlLayers=[];    // active query-match layer ids
let layerPalette={};// classless layer colour per layer name (captured at sync)
let hlCounter=0;
// Selection + highlight overlay layers, tracked so they can be removed when
// their layer/sublayer is toggled OFF (otherwise the always-on-top overlay
// keeps showing features of a disabled sublayer).
let overlayMeta=[]; // {id, source:'spire-<name>', cls}
// Click-cycling state: repeat clicks at ~the same pixel step through the
// stacked features underneath (instead of always picking the topmost).
let lastPick={key:'',at:0,idx:0};

function detachOverlays(ids){
  const list=ids.slice();
  for(const id of list){
    try{ if(map.getLayer(id)) map.removeLayer(id); }catch(e){}
    let i=selLayers.indexOf(id); if(i>=0) selLayers.splice(i,1);
    i=hlLayers.indexOf(id); if(i>=0) hlLayers.splice(i,1);
  }
  if(list.length) overlayMeta = overlayMeta.filter(o=>list.indexOf(o.id)<0);
}
function dropOverlaysFor(source, clsKey){
  // clsKey === null → whole layer; otherwise a single class (sublayer).
  const doomed = overlayMeta.filter(o=> o.source===source && (clsKey===null || o.cls===clsKey));
  if(doomed.length) detachOverlays(doomed.map(o=>o.id));
}
function layerGeomKind(geomType){
  if(geomType==='Point'||geomType==='MultiPoint') return 'circle';
  if(geomType.indexOf('Line')>=0) return 'line';
  return 'fill';
}
// Paints for match/selection layers: same class/layer colours as the base
// layers, slightly emphasised so matches stand out on top of everything.
function emphPaint(kind, clsKey, layerName, emphasize){
  if(kind==='line'){
    const s = LINE_STYLES.find(x=>x.key===clsKey) || {color:'#a5a5a5',width:1.5,opacity:1};
    return {'line-color':s.color,
            'line-width':emphasize?Math.max(3,s.width+2.5):s.width+1.5,
            'line-opacity':1};
  }
  if(kind==='fill'){
    const s = POLY_STYLES.find(x=>x.key===clsKey) || {color:'#e6d8a8',opacity:0.5};
    return {'fill-color':s.color,
            'fill-opacity':emphasize?Math.min(1,s.opacity+0.3):0.85,
            'fill-outline-color':'#000000'};
  }
  const c = layerPalette[layerName] || '#f59e0b';
  return {'circle-color':c,
          'circle-radius':emphasize?8:9,
          'circle-stroke-color':'#ffffff',
          'circle-stroke-width':1};
}
function clearMapSelection(){
  detachOverlays(selLayers);
}
// Selection mode: 'object' (whole feature) or 'point' (nearest vertex of the
// top feature under the cursor). Point mode has no Street View.
const selectionMode={mode:'object'};
window.spireSetSelectionMode=function(m){
  selectionMode.mode = (m==='point')?'point':'object';
  try{ map.getCanvas().style.cursor = (selectionMode.mode==='point') ? 'default' : ''; }
  catch(e){}
};
// Active selection layer set from the sidebar: when set, clicks only resolve
// against that entry (one layer, or a single sublayer), so overlapping layers
// can't steal the pick. Empty = auto (all visible layers, with cycling).
const activeSelection={layer:'',cls:null};
window.spireSetActiveLayer=function(name,cls){
  activeSelection.layer = name||'';
  activeSelection.cls = (cls && cls.length) ? cls : null;
};
function clickHitLayers(){
  if(!activeSelection.layer) return selectableLayers;
  const base='spire-'+activeSelection.layer+'-layer';
  const pre='spire-'+activeSelection.layer+'-cls:';
  const ids=[];
  selectableLayers.forEach(id=>{
    if(activeSelection.cls){ if(id===pre+activeSelection.cls) ids.push(id); }
    else if(id===base || id.indexOf(pre)===0) ids.push(id);
  });
  return ids.length ? ids : selectableLayers;
}
function geometryCoords(g){
  if(!g||!g.coordinates) return [];
  const t=g.type, c=g.coordinates, pts=[];
  if(t==='Point') pts.push(c);
  else if(t==='MultiPoint'||t==='LineString') c.forEach(p=>pts.push(p));
  else if(t==='MultiLineString'||t==='Polygon') c.forEach(ring=>ring.forEach(p=>pts.push(p)));
  else if(t==='MultiPolygon') c.forEach(poly=>poly.forEach(ring=>ring.forEach(p=>pts.push(p))));
  return pts;
}
function nearestVertex(points,lng,lat){
  let best=null,bd=Infinity;
  const k=Math.cos(lat*Math.PI/180);
  for(let i=0;i<points.length;i++){
    const dx=(points[i][0]-lng)*k, dy=(points[i][1]-lat);
    const d=dx*dx+dy*dy;
    if(d<bd){ bd=d; best={coord:points[i], index:i}; }
  }
  return best;
}
function ensurePointLayer(){
  try{
    if(!map.getSource('spire-pointsel')){
      map.addSource('spire-pointsel',{type:'geojson',data:{type:'FeatureCollection',features:[]}});
    }
    if(!map.getLayer('spire-pointsel-layer')){
      map.addLayer({id:'spire-pointsel-layer',type:'circle',source:'spire-pointsel',
        paint:{'circle-radius':6,'circle-color':'#ff3b30',
               'circle-stroke-color':'#ffffff','circle-stroke-width':2}});
    }
  }catch(e){ post({kind:'log',text:'point layer: '+e.message}); }
}
function showPoint(lng,lat){
  ensurePointLayer();
  const src=map.getSource('spire-pointsel');
  if(src) src.setData({type:'FeatureCollection',features:[
    {type:'Feature',geometry:{type:'Point',coordinates:[lng,lat]},properties:{}}]});
}
function clearPoint(){
  const src=map.getSource('spire-pointsel');
  if(src) src.setData({type:'FeatureCollection',features:[]});
}
map.on('click',(e)=>{
  // Only the visible base layers are clickable; overlay layers (selection +
  // query matches) sit on top but are NOT part of the pick stack, so cycling
  // steps through the real features underneath. Overlays are dropped when a
  // layer is toggled off, so nothing hidden can be selected.
  const feats = map.queryRenderedFeatures(e.point,{layers:clickHitLayers()});
  if(!feats || feats.length===0){
    lastPick.at=0; clearMapSelection();
    if(selectionMode.mode==='point'){
      showPoint(e.lngLat.lng,e.lngLat.lat);
      post({kind:'select', point:true, layer:'', name:'', objectid:'', class:'', id:'', vertex:-1,
            stacked:1, pick:0, lat:e.lngLat.lat, lng:e.lngLat.lng});
    } else {
      post({kind:'log',text:'click: 0 features under cursor'});
      post({kind:'select',empty:true});
    }
    return;
  }
  // Repeated clicks at (roughly) the same point cycle through the stack;
  // clicking anywhere else (or after a pause) starts again from the top.
  const key = Math.round(e.point.x)+','+Math.round(e.point.y);
  const now = Date.now();
  let idx = 0;
  if(lastPick.at && lastPick.key===key && (now-lastPick.at) < 1500 && feats.length>1){
    idx = (lastPick.idx + 1) % feats.length;
  }
  lastPick.key=key; lastPick.at=now; lastPick.idx=idx;
  const f=feats[idx];
  const p=f.properties||{};
  const pid = (f.id!=null) ? f.id : ((p.id!=null) ? p.id : null);
  post({kind:'log',text:'click: '+feats.length+' feat(s), picked '+(f.source||'?')+' pid='+pid});
  if(selectionMode.mode==='point'){
    const pts=geometryCoords(f.geometry);
    const nv=nearestVertex(pts, e.lngLat.lng, e.lngLat.lat);
    const plng = nv?nv.coord[0]:e.lngLat.lng;
    const plat = nv?nv.coord[1]:e.lngLat.lat;
    clearMapSelection();
    showPoint(plng,plat);
    post({kind:'select', point:true, layer:(f.source||'').replace('spire-',''),
          name:(p.name||p.NAME||''),
          objectid:(p.objectid!=null)?p.objectid:((p.OBJECTID!=null)?p.OBJECTID:''),
          class:p.class||'', id:'', vertex:(nv?nv.index:-1),
          stacked:feats.length, pick:idx, lat:plat, lng:plng});
    return;
  }
  clearPoint();
  if(!f.source || pid==null){ clearMapSelection(); post({kind:'select',empty:true}); return; }
  clearMapSelection();
  // Decide the geometry kind from the style layer the feature came from.
  let kind='fill';
  try{ const l=map.getLayer(f.layer && f.layer.id); if(l) kind=l.type; }catch(err){}
  if(kind!=='circle' && kind!=='line') kind='fill';
  const selId='spire-sel-'+(f.source).replace(/[^A-Za-z0-9]/g,'_')+'-'+hlCounter++;
  try{
    map.addLayer({id:selId,type:kind,source:f.source,
      paint:emphPaint(kind, p.class||p.FOLDERPATH||'', (f.source||'').replace('spire-',''), false),
      filter:['==',['get','id'],pid]});
    selLayers.push(selId);
    overlayMeta.push({id:selId, source:f.source, cls:p.class||p.FOLDERPATH||''});
  }catch(err){ post({kind:'log',text:'select-layer: '+(err&&err.message||err)}); }
  const name = p.name || p.NAME || '';
  const objectid = (p.objectid!=null)?p.objectid:((p.OBJECTID!=null)?p.OBJECTID:'');
  const cls = p.class || '';
  const layer = (f.source||'').replace('spire-','');
  post({kind:'select', layer, name:name||'', objectid:objectid||'', class:cls||'', id: pid,
        geom: kind, stacked: feats.length, pick: idx,
        lat: e.lngLat && e.lngLat.lat, lng: e.lngLat && e.lngLat.lng});
});
window.spireClearSelection = function(){ clearMapSelection(); clearPoint(); };
map.on('moveend',()=>{
  const b=map.getBounds();
  post({kind:'viewport',minLng:b.getWest(),minLat:b.getSouth(),maxLng:b.getEast(),maxLat:b.getNorth()});
});
const pending = {};
maplibregl.addProtocol('spire',(params,callback)=>{
  try{
    const p = params.url.replace('spire://','').split('/'); // tiles/<layer>/<z>/<x>/<y>
    pending[params.url] = callback;
    post({kind:'tile',url:params.url,layer:decodeURIComponent(p[1]),z:+p[2],x:+p[3],y:+p[4]});
  }catch(e){ callback(new Error('spire protocol '+e)); }
});
window.spireResolveTile = function(url,b64){
  const cb = pending[url]; if(!cb) return; delete pending[url];
  try{
    const bin = atob(b64); const buf = new Uint8Array(bin.length);
    for(let i=0;i<bin.length;i++) buf[i]=bin.charCodeAt(i);
    cb(null,buf.buffer);
  }catch(e){ cb(new Error('tile decode '+e)); }
};
function paintFor(geom,color){
  if(geom==='Point') return {type:'circle',paint:{'circle-color':color,'circle-radius':5,'circle-stroke-width':1,'circle-stroke-color':'#ffffff'}};
  if(geom==='LineString') return linePaint();
  return polygonPaint();
}
// Base layers render with PLAIN paints (no feature-state): MapLibre 4.7.1 in
// this WKWebView throws when a layer painted with a data-driven expression is
// flipped to visible via setLayoutProperty. Selection + query matches are
// drawn by temporary companion layers (added on demand, same colours).
function classStyle(geom,key){
  if(geom.indexOf('Line')>=0){
    const s = LINE_STYLES.find(x=>x.key===key) || {key:key,color:'#a5a5a5',width:1.5,opacity:1};
    return {type:'line',paint:{
      'line-color':s.color,
      'line-width':s.width,
      'line-opacity':s.opacity}};
  }
  const s = POLY_STYLES.find(x=>x.key===key) || {key:key,color:'#e6d8a8',opacity:0.5};
  return {type:'fill',paint:{
    'fill-color':s.color,
    'fill-opacity':s.opacity,
    'fill-outline-color':'#8a8a8a'}};
}
function kindType(geom){
  if(geom==='Point'||geom==='MultiPoint') return 'circle';
  if(geom.indexOf('Line')>=0) return 'line';
  return 'fill';
}
function knownOrder(geom){
  const styles = geom.indexOf('Line')>=0 ? LINE_STYLES : POLY_STYLES;
  return styles.map(s=>s.key);
}
// Deliberate colours for classless (non-FOLDERPATH) layers — greening the
// eco/NParks datasets. National-map layers stay on their class styles.
const LAYER_COLORS = {
  'nparks-nature-reserves':    '#2d6a4f',  // deep forest green
  'parks':                     '#52b788',  // park green
  'heritage-trees':            '#40916c',  // tree green
  'park-connector-loop':       '#95d5b2',  // light connector green
  'tree-conservation-area':    '#1b4332',  // very dark green
  'heritage-road-green-buffers':'#a7c957', // yellow-green
  'nparks-tracks':             '#4d908e',  // teal
  'community-in-bloom':        '#d81b60',  // blossom pink
  'natureways':                '#80b918',  // vivid lime green
  'shoreline-typology':        '#0077b6',  // coastal blue
};
function layerColor(name, idx){
  const c = LAYER_COLORS[name];
  return c || PALETTE[idx % PALETTE.length];
}
function plainStyle(geom,color){
  if(geom==='Point'||geom==='MultiPoint'){
    return {type:'circle',paint:{
      'circle-color':color,
      'circle-radius':5,
      'circle-stroke-width':1,'circle-stroke-color':'#ffffff'}};
  }
  if(geom.indexOf('Line')>=0){
    return {type:'line',paint:{
      'line-color':color,
      'line-width':2.2,
      'line-opacity':0.9}};
  }
  return {type:'fill',paint:{
    'fill-color':color,
    'fill-opacity':0.5,
    'fill-outline-color':'#555555'}};
}
window.spireSetLayers = function(jsonStr){
  let list=[]; try{ list = JSON.parse(jsonStr); }catch(e){ post({kind:'log',text:'spireSetLayers parse: '+e.message}); return; }
  let hasLines=false;
  let hasAreas=false;
  try{
    list.forEach((L,idx)=>{
      const nm='spire-'+L.name;
      if(map.getSource(nm)) return;
      const kt = kindType(L.geometry_type);
      if(kt==='line') hasLines=true;
      if(kt==='fill') hasAreas=true;
      // GeoJSON source: parsed on the main thread (no worker), so it renders in
      // WKWebView regardless of worker availability.
      map.addSource(nm,{type:'geojson',promoteId:'id',data:{type:'FeatureCollection',features:[]}});
      const vis = (L.visible===true?'visible':'none');
      // Layers with no FOLDERPATH-style classes (many data.gov.sg layers)
      // render as ONE generic style layer in a palette colour.
      const clsList = (L.classes||[]).map(c=>c.key);
      if(clsList.length===0){
        layerPalette[L.name]=layerColor(L.name, idx);
        const s = plainStyle(L.geometry_type, layerColor(L.name, idx));
        const id = nm+'-layer';
        if(!map.getLayer(id)){
          map.addLayer({id:id,type:s.type,source:nm,
            layout:{visibility:vis},paint:s.paint});
          if(!selectableLayers.includes(id)) selectableLayers.push(id);
        }
        return;
      }
      // Classified layers: one MapLibre layer per class so each type can be
      // toggled independently. Known styles keep a stable z-order, unknown
      // classes are appended.
      const keys = knownOrder(L.geometry_type).filter(k=>clsList.indexOf(k)>=0);
      clsList.forEach(k=>{ if(keys.indexOf(k)<0) keys.push(k); });
      keys.forEach(key=>{
        const style = classStyle(L.geometry_type,key);
        const id = nm+'-cls:'+key;
        if(!map.getLayer(id)){
          // Everything starts OFF: checkboxes are unchecked and sublayers are
          // hidden until the user switches them on.
          map.addLayer({id:id,type:style.type,source:nm,
            filter:['==',['get','class'],key],
            layout:{visibility:'none'},paint:style.paint});
          if(!selectableLayers.includes(id)) selectableLayers.push(id);
        }
      });
    });
  }catch(e){ post({kind:'log',text:'spireSetLayers: '+e.message}); }
  renderLegend(hasLines, hasAreas);
};
window.spireSetLayerData = function(name,jsonStr){
  try{
    const src = map.getSource('spire-'+name);
    if(!src){ post({kind:'log',text:'setData '+name+': source not found'}); return; }
    const fc = JSON.parse(jsonStr);
    if(!fc || !fc.features){ post({kind:'log',text:'setData '+name+': bad payload'}); return; }
    src.setData(fc);
    post({kind:'log',text:name+': '+fc.features.length+' features loaded'});
  }catch(e){ post({kind:'log',text:'setData '+name+': '+e.message}); }
};
window.spireSetLayerDataB64 = function(name,b64){
  try{
    const src = map.getSource('spire-'+name);
    if(!src){ post({kind:'log',text:'setData '+name+': source not found'}); return; }
    const fc = JSON.parse(atob(b64));
    if(!fc || !fc.features){ post({kind:'log',text:'setData '+name+': bad payload'}); return; }
    src.setData(fc);
    post({kind:'log',text:name+': '+fc.features.length+' features loaded'});
  }catch(e){ post({kind:'log',text:'setDataB64 '+name+': '+e.message}); }
};
window.spireSetVisibility = function(name,visible){
  try{
    const v = visible?'visible':'none';
    const source='spire-'+name;
    if(!visible) dropOverlaysFor(source, null);
    const prefix=source+'-cls:';
    const style=map.getStyle();
    let any=false;
    ((style && style.layers)||[]).forEach(l=>{
      if(l.id && l.id.indexOf(prefix)===0){ map.setLayoutProperty(l.id,'visibility',v); any=true; }
    });
    if(!any){ const id=source+'-layer'; if(map.getLayer(id)) map.setLayoutProperty(id,'visibility',v); }
  }catch(e){ post({kind:'log',text:'setVisibility '+name+': '+e.message}); }
};
window.spireSetClassVisibility = function(name,key,visible){
  try{
    const source='spire-'+name;
    const id=source+'-cls:'+key;
    if(!visible) dropOverlaysFor(source, key);
    if(map.getLayer(id)) map.setLayoutProperty(id,'visibility',visible?'visible':'none');
  }catch(e){ post({kind:'log',text:'setClassVisibility '+name+': '+e.message}); }
};
// Re-stack the data layers to match `names` (bottom-to-top; later = on top).
// Overlay layers (selection + query matches) are re-raised so they stay above.
window.spireOrderLayers = function(namesJson){
  try{
    let names=[]; try{ names=JSON.parse(namesJson); }catch(e){ return; }
    const styleLayers=(map.getStyle()&&map.getStyle().layers)||[];
    const allIds=styleLayers.map(l=>l.id);
    const ordered=[];
    names.forEach(n=>{
      const base='spire-'+n+'-layer';
      const pre='spire-'+n+'-cls:';
      allIds.forEach(id=>{ if(id===base || id.indexOf(pre)===0) ordered.push(id); });
    });
    ordered.forEach(id=>{ if(map.getLayer(id)) map.moveLayer(id); });
    hlLayers.concat(selLayers).forEach(id=>{ if(map.getLayer(id)) map.moveLayer(id); });
    if(map.getLayer('spire-pointsel-layer')) map.moveLayer('spire-pointsel-layer');
  }catch(e){ post({kind:'log',text:'orderLayers: '+e.message}); }
};
// --- Query-result matching (temporary companion layers on top) ---
function clearHighlights(){ detachOverlays(hlLayers); }
window.spireClearResults = function(){ clearHighlights(); };
window.spireHighlightResults = function(fcJson){
  clearHighlights();
  if(!fcJson) return;
  let fc; try{ fc=JSON.parse(fcJson); }catch(e){ post({kind:'log',text:'highlight parse: '+e.message}); return; }
  // Group matched features by (layer, geometry kind, class) so each group gets
  // one always-on-top layer painted with the layer/class colour. No feature
  // state and no visibility toggling — nothing for MapLibre to mis-handle.
  const groups={};
  (fc.features||[]).forEach(f=>{
    const p=f.properties||{};
    const layer=p.layer || '';
    if(!layer) return;
    const id = (f.id!=null)?f.id:((p.id!=null)?p.id:null);
    if(id==null) return;
    const geom=(f.geometry&&f.geometry.type)||p.geometry_type||'';
    const kind=layerGeomKind(geom);
    const cls=p.FOLDERPATH || p.class || '';
    const key=layer+'||'+kind+'||'+cls;
    (groups[key]=groups[key]||{layer,kind,cls,ids:[]}).ids.push(id);
  });
  Object.keys(groups).forEach(key=>{
    const g=groups[key];
    const src='spire-'+g.layer;
    let has=false;
    try{ has = !!map.getSource(src); }catch(e){ post({kind:'log',text:'getSource '+src+': '+(e&&e.message||e)}); }
    if(!has) { post({kind:'log',text:'hl: no source for '+g.layer}); return; }
    const id='spire-hl-'+g.layer.replace(/[^A-Za-z0-9]/g,'_')+'-'+g.kind+'-'+hlCounter++;
    try{
      map.addLayer({id,type:g.kind,source:src,
        paint:emphPaint(g.kind, g.cls, g.layer, true),
        filter:['in',['get','id'],['literal',g.ids]]});
      hlLayers.push(id);
      overlayMeta.push({id, source:src, cls:g.cls});
    }catch(e){ post({kind:'log',text:'hl add '+id+': '+(e&&e.message||e)}); }
  });
  post({kind:'log',text:'highlight: '+hlLayers.length+' match layer(s) added'});
};
window.spireReportBounds = function(){
  const b = map.getBounds();
  post({kind:'bounds',minLng:b.getWest(),minLat:b.getSouth(),maxLng:b.getEast(),maxLat:b.getNorth()});
};
window.spireFitBounds = function(b){
  if(!map || b.length!==4) return;
  try{ map.fitBounds([[b[0],b[1]],[b[2],b[3]]],{padding:30,duration:600}); }catch(e){}
};
map.on('load',()=>post({kind:'ready'}));
map.on('error',(e)=>post({kind:'log',text:'maplibre: '+(e && (e.error && e.error.message || e.message) || '')}));
</script></body></html>
"""#
