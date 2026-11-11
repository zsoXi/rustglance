import json,queue,subprocess,threading,time
class RG:
 def __init__(self,exe,config):
  self.p=subprocess.Popen([exe,'mcp','--config',config],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL)
  self.q=queue.Queue();self.seq=0
  def read():
   for line in self.p.stdout:self.q.put(line)
   self.q.put(None)
  threading.Thread(target=read,daemon=True).start()
  self.rpc('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'rg-bench','version':'1'}})
  self.p.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n');self.p.stdin.flush()
 def rpc(self,method,params):
  self.seq+=1;self.p.stdin.write((json.dumps({'jsonrpc':'2.0','id':self.seq,'method':method,'params':params})+'\n').encode());self.p.stdin.flush()
  deadline=time.monotonic()+45
  while True:
   line=self.q.get(timeout=max(.01,deadline-time.monotonic()))
   if line is None:raise RuntimeError('RG stdout closed')
   r=json.loads(line)
   if r.get('id')==self.seq:return r
 def call(self,name,args):
  t=time.perf_counter();r=self.rpc('tools/call',{'name':name,'arguments':args});elapsed=(time.perf_counter()-t)*1000
  result=r.get('result',{});content=result.get('content',[])
  data=json.loads(content[0]['text']) if content else {}
  return {'tool':name,'wall_ms':round(elapsed,2),'error':result.get('isError',False),'data':data,'text':'\n'.join(x.get('text','') for x in content[1:] if x.get('type')=='text')}
 def close(self):
  self.p.stdin.close()
  try:self.p.wait(timeout=8)
  except subprocess.TimeoutExpired:self.p.terminate();self.p.wait(timeout=5)
