import Foundation

/// The map page loaded into the WKWebView: MapLibre GL JS (from CDN) with a
/// light raster base map, one vector source per imported layer (fed through
/// the `spire://` custom protocol → Swift → `gis/get-tile`), and a GeoJSON
/// source for spatial-query results.
///
/// JS → Swift messages are posted to `window.webkit.messageHandlers.spireBridge`
/// (`ready`, `tile`, `bounds`, `log`); Swift → JS uses `window.spire*` fns.
let spireMapHtml = #"""
<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<link href="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.css" rel="stylesheet">
<script src="https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.js"></script>
<style>html,body,#m{margin:0;height:100%;width:100%;overflow:hidden}</style></head>
<body><div id="m"></div>
<script>
const post = (o)=>window.webkit && window.webkit.messageHandlers &&
  window.webkit.messageHandlers.spireBridge && window.webkit.messageHandlers.spireBridge.postMessage(o);
const PALETTE = ['#e6194b','#3cb44b','#4363d8','#f58231','#911eb4','#42d4f4','#f032e6'];
const style = {
  version:8,
  sources:{
    base:{type:'raster',tileSize:256,
      tiles:['https://a.basemaps.cartocdn.com/light_all/{z}/{x}/{y}@2x.png',
             'https://b.basemaps.cartocdn.com/light_all/{z}/{x}/{y}@2x.png',
             'https://c.basemaps.cartocdn.com/light_all/{z}/{x}/{y}@2x.png'],
      attribution:'&copy; OpenStreetMap &copy; CARTO'},
    results:{type:'geojson',data:{type:'FeatureCollection',features:[]}}
  },
  layers:[
    {id:'base',type:'raster',source:'base'},
    {id:'results-fill',type:'fill',source:'results',paint:{'fill-color':'#e6194b','fill-opacity':0.3}},
    {id:'results-line',type:'line',source:'results',paint:{'line-color':'#e6194b','line-width':2}},
    {id:'results-circle',type:'circle',source:'results',paint:{'circle-color':'#e6194b','circle-radius':6}}
  ]
};
const map = new maplibregl.Map({container:'m',style:style,center:[103.85,1.35],zoom:11});
window.map = map;
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
  if(geom==='LineString') return {type:'line',paint:{'line-color':color,'line-width':2.5}};
  return {type:'fill',paint:{'fill-color':color,'fill-opacity':0.45,'fill-outline-color':'#222222'}};
}
window.spireSetLayers = function(jsonStr){
  let list=[]; try{ list = JSON.parse(jsonStr); }catch(e){ return; }
  list.forEach((L,idx)=>{
    const nm='spire-'+L.name;
    if(map.getSource(nm)) return;
    const color = PALETTE[idx % PALETTE.length];
    const s = paintFor(L.geometry_type,color);
    // GeoJSON source: parsed on the main thread (no worker), so it renders in
    // WKWebView regardless of worker availability.
    map.addSource(nm,{type:'geojson',data:{type:'FeatureCollection',features:[]}});
    map.addLayer({id:nm+'-layer',type:s.type,source:nm,
      layout:{visibility:(L.visible===false?'none':'visible')},paint:s.paint});
  });
};
window.spireSetLayerData = function(name,jsonStr){
  try{
    const src = map.getSource('spire-'+name); if(!src) return;
    const fc = JSON.parse(jsonStr);
    if(!fc || !fc.features) return;
    src.setData(fc);
    post({kind:'log',text:name+': '+fc.features.length+' features loaded'});
  }catch(e){ post({kind:'log',text:'setData '+name+': '+e}); }
};
window.spireSetVisibility = function(name,visible){
  const id='spire-'+name+'-layer';
  if(map.getLayer(id)) map.setLayoutProperty(id,'visibility',visible?'visible':'none');
};
window.spireSetResults = function(fcJson){
  let show=false;
  try{ if(fcJson){ const fc=JSON.parse(fcJson); show=(fc.features && fc.features.length>0);
    map.getSource('results').setData(fc); } else { map.getSource('results').setData({type:'FeatureCollection',features:[]}); } }catch(e){}
  const v = show?'visible':'none';
  map.setLayoutProperty('results-fill','visibility',v);
  map.setLayoutProperty('results-line','visibility',v);
  map.setLayoutProperty('results-circle','visibility',v);
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
