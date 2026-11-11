from http.server import ThreadingHTTPServer,BaseHTTPRequestHandler
from pathlib import Path
import threading,time,json,argparse,tempfile
from rg_client import RG
parser=argparse.ArgumentParser(description="Local Chromium readiness regression; never uses personal profiles or public sites")
parser.add_argument('--exe',required=True);parser.add_argument('--chromium',required=True);parser.add_argument('--output',required=True)
options=parser.parse_args()
B=Path(options.output).resolve();B.mkdir(parents=True,exist_ok=True)
temporary=tempfile.TemporaryDirectory(prefix='rustglance-ready-');temporary_path=Path(temporary.name)
hits=[]
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def do_POST(self):
  hits.append(self.path);self.send_response(200);self.end_headers()
 def do_GET(self):
  if self.path=='/slow':
   time.sleep(4);data=b'ok';ctype='text/plain'
  else:
   ctype='text/html';page=self.path
   if page=='/fixture':
    data=b'''<html><body><p id="noise">tick</p><a href="/destination">Destination</a><button onclick="fetch('/write',{method:'POST'})">Send test POST</button><script>fetch('/slow');setInterval(()=>document.querySelector('#noise').textContent=Date.now(),70);setTimeout(()=>{let e=document.createElement('main');e.innerHTML='<p>Useful stable content for a real task.</p>';document.body.append(e)},350)</script></body></html>'''
   elif page=='/destination':data=b'<html><body><main><p>Destination useful content ready.</p></main></body></html>'
   elif page=='/prose':data=b'<html><body><p><a href="/destination">Artificial intelligence</a> (AI) is the capability of <a href="/destination">computers</a> to perform tasks.</p></body></html>'
   elif page=='/select':data=b'<html><body><label>Disabled<select disabled><option>A</option><option>B</option></select></label><label>Choices<select><option>A</option><option disabled>B</option><option>C</option><optgroup disabled label="Off"><option>D</option></optgroup></select></label><label>Locked<input readonly value="unchanged"></label></body></html>'
   elif page=='/hidden':data=b'<html><body><main style="opacity:0"><p>Hidden useful content</p></main></body></html>'
   elif page=='/flicker':data=b'<html><body><main><p>Changing 0</p></main><script>let i=0;setInterval(()=>document.querySelector("p").textContent="Changing "+(++i),50)</script></body></html>'
   elif page=='/busy':data=b'<html><body><main><p>Busy document</p></main><script>setTimeout(()=>{let t=Date.now();while(Date.now()-t<1200){}},0)</script></body></html>'
   else:data=b'<html><body>No matching useful content</body></html>'
  self.send_response(200);self.send_header('Content-Type',ctype);self.send_header('Content-Length',str(len(data)));self.end_headers()
  try:self.wfile.write(data)
  except (BrokenPipeError,ConnectionAbortedError,ConnectionResetError):pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
base=f'http://127.0.0.1:{server.server_port}'
cfg=temporary_path/'config.json'
cfg.write_text(json.dumps({'chromium':str(Path(options.chromium).resolve()),'profile':str(temporary_path/'profile'),'evidence':str(B/'evidence'),'headless':True,'start_url':'about:blank','writes':False,'write_block':['*']}))
r=RG(str(Path(options.exe).resolve()),str(cfg));results=[]
def test(name,op,args,predicate):
 v=r.call(op,args);ok=predicate(v);results.append({'test':name,'pass':ok,'result':v});print(name,ok,v['wall_ms'],flush=True);assert ok,name
try:
 r.call('open',{'url':base+'/prose'})
 test('inline links preserve prose order','tree',{},lambda v:'Artificial intelligence (AI) is the capability of computers to perform tasks.' in v['text'] and v['text'].count('link ')>=2)
 r.call('open',{'url':base+'/select'})
 import re
 tree=r.call('tree',{})['text']; refs=[re.search(r'\be\d+\b',line).group() for line in tree.splitlines() if 'combobox' in line]
 assert 'readonly' in tree and 'disabled' in tree, 'control restrictions must be visible to the agent'
 locked=next(re.search(r'\be\d+\b',line).group() for line in tree.splitlines() if 'textbox' in line)
 test('disabled select refuses','type',{'ref':refs[0],'text':'B'},lambda v:v['error'] and v['data']['error']=='not_found')
 test('disabled option refuses','type',{'ref':refs[1],'text':'B'},lambda v:v['error'] and v['data']['error']=='not_found')
 test('disabled optgroup refuses','type',{'ref':refs[1],'text':'D'},lambda v:v['error'] and v['data']['error']=='not_found')
 test('readonly input refuses','type',{'ref':locked,'text':'replacement'},lambda v:v['error'] and v['data']['error']=='not_found')
 test('enabled option works','type',{'ref':refs[1],'text':'C'},lambda v:not v['error'] and v['data'].get('value')=='C')
 test('default open result preserves prior shape','open',{'url':base+'/destination'},lambda v:not v['error'] and 'readiness' not in v['data'] and isinstance(v['data'].get('settled'),bool))
 test('useful content ignores unrelated DOM/network churn','open',{'url':base+'/fixture','wait_for':{'selector':'main p','min_text_length':20}},lambda v:not v['error'] and v['data']['readiness']['ready'] and v['wall_ms']<3000)
 test('invalid selector refuses before navigation','open',{'url':base+'/destination','wait_for':{'selector':'['}},lambda v:v['error'] and v['data']['error']=='bad_args')
 test('invalid input left page unchanged','check',{'url':'/fixture'},lambda v:v['data'].get('pass') is True)
 test('invalid selector refuses before click','tap',{'text':'Destination','wait_for':{'selector':'['}},lambda v:v['error'] and v['data']['error']=='bad_args')
 test('invalid click condition leaves source page','check',{'url':'/fixture'},lambda v:v['data'].get('pass') is True)
 test('real click waits for destination content','tap',{'text':'Destination','wait_for':{'url':'/destination','selector':'main p','min_text_length':20}},lambda v:not v['error'] and v['data']['readiness']['ready'])
 test('missing condition refuses','open',{'url':base+'/missing','wait_for':{'selector':'main p','timeout_ms':500}},lambda v:v['error'] and v['data']['error']=='timeout')
 test('hidden content cannot pass','open',{'url':base+'/hidden','wait_for':{'selector':'main','timeout_ms':500}},lambda v:v['error'] and v['data']['error']=='timeout')
 test('hidden ancestor cannot pass child selector','open',{'url':base+'/hidden','wait_for':{'selector':'main p','timeout_ms':500}},lambda v:v['error'] and v['data']['error']=='timeout')
 test('hidden text cannot satisfy text condition','open',{'url':base+'/hidden','wait_for':{'text':'Hidden useful content','timeout_ms':500}},lambda v:v['error'] and v['data']['error']=='timeout')
 test('changing relevant content is not stable','open',{'url':base+'/flicker','wait_for':{'selector':'main p','timeout_ms':500}},lambda v:v['error'] and v['data']['error']=='timeout')
 test('short wait budget does not classify busy page as wedged','open',{'url':base+'/busy','wait_for':{'selector':'main p','timeout_ms':100}},lambda v:v['error'] and v['data']['error']=='timeout')
 test('busy timeout keeps page instead of recovering tab','check',{'url':'/busy','text':'Busy document','timeout_ms':3000},lambda v:v['data'].get('pass') is True)
 test('navigation cannot reuse previous content','open',{'url':base+'/destination','wait_for':{'selector':'main p','min_text_length':20}},lambda v:not v['error'])
 test('same selector absent in next document must timeout','open',{'url':base+'/missing','wait_for':{'selector':'main p','timeout_ms':500}},lambda v:v['error'])
 r.call('open',{'url':base+'/fixture','wait_for':{'selector':'main p'}})
 test('POST stays blocked in condition mode','tap',{'text':'Send test POST','wait_for':{'selector':'main p'}},lambda v:not v['error'] and bool(v['data'].get('blocked_writes')) and not hits)
except Exception as e:results.append({'failure':str(e)});raise
finally:
 (B/'runtime-regression.json').write_text(json.dumps({'tests':results,'received_posts':hits},indent=2),encoding='utf-8');r.close();server.shutdown();temporary.cleanup()
