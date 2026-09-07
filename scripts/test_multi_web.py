#!/usr/bin/env python3
"""Stdlib security and lifecycle tests for multi_web."""
import importlib.util, json, os, socket, subprocess, tempfile, unittest
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen
SPEC=importlib.util.spec_from_file_location('multi',Path(__file__).with_name('multi_web.py')); multi=importlib.util.module_from_spec(SPEC);SPEC.loader.exec_module(multi)
def free():
 s=socket.socket();s.bind(('127.0.0.1',0));p=s.getsockname()[1];s.close();return p
class Fake:
 def __init__(self,argv,cwd,**kw):self.argv,self.cwd,self.kw=argv,cwd,kw;self.dead=False;self.terminated=self.killed=self.waits=0
 def poll(self):return 0 if self.dead else None
 def terminate(self):self.terminated+=1
 def kill(self):self.killed+=1;self.dead=True
 def wait(self,timeout=None):
  self.waits+=1
  if self.dead:return 0
  if timeout is None: self.dead=True;return 0
  raise subprocess.TimeoutExpired(self.argv,timeout)
class Base:
 def setUp(self):
  self.t=tempfile.TemporaryDirectory();self.r=Path(self.t.name);(self.r/'a dir').mkdir();(self.r/'b').mkdir();self.exe=self.r/'native tool';self.exe.write_text('#!/bin/sh\nexit 0');self.exe.chmod(0o755)
 def tearDown(self):self.t.cleanup()
 def raw(self,**kw):
  x={'landing_port':19001,'open_browser':False,'primary':'a','command':'./native tool','servers':[{'id':'a','label':'A','cwd':'a dir','port':19002,'enabled':True,'profile':'p','args':['--read-only']},{'id':'b','label':'B','cwd':'b','port':19003,'enabled':False,'args':[]}]};x.update(kw);return x
 def conf(self,**kw):p=self.r/'c.json';p.write_text(json.dumps(self.raw(**kw)));return p
 def load(self,**kw):return multi.load_config(self.conf(**kw))
 def _test_validation_matrix(self):
  self.assertEqual(self.load()['command'],str(self.exe.resolve()))
  bad=['{"servers":[],"servers":[]}','{"servers":NaN}','[]']
  for raw in bad:
   path=self.r/'malformed.json';path.write_text(raw)
   with self.assertRaises(multi.ConfigError): multi.load_config(path)
  muts=[lambda x:x.update(x=1),lambda x:x.update(open_browser=1),lambda x:x['servers'][1].update(x=1),lambda x:x['servers'][1].update(enabled='yes'),lambda x:x['servers'][0].update(port=True),lambda x:x['servers'][0].update(port=0),lambda x:x['servers'][0].update(port=65536),lambda x:x['servers'][0].update(id='__proto__'),lambda x:x['servers'][0].update(label=' bad'),lambda x:x['servers'][0].update(cwd='nope'),lambda x:x['servers'][0].update(profile='x\0'),lambda x:x['servers'][0].update(args=['--host=x']),lambda x:x['servers'][1].update(enabled=True,id='a'),lambda x:x['servers'][1].update(enabled=True,port=19002),lambda x:x['servers'][1].update(enabled=True,cwd='a dir'),lambda x:x.update(landing_port=19002),lambda x:x.update(primary='b'),lambda x:[z.update(enabled=False) for z in x['servers']]]
  for m in muts:
   x=self.raw();m(x)
   with self.assertRaises(multi.ConfigError):multi.load_config(self.conf(**x))
  for n in (33,):
   x=self.raw(servers=[{'id':'x%d'%i,'label':'x','cwd':'a dir' if i==0 else 'b','port':20000+i,'enabled':False,'args':[]} for i in range(n)])
   with self.assertRaises(multi.ConfigError):multi.load_config(self.conf(**x))
  for ext in ('.cmd','.bat'):
   q=self.r/('x'+ext);q.write_text('x');q.chmod(0o755)
   with self.assertRaises(multi.ConfigError):self.load(command='./'+q.name)
 def _test_sequential_ready_timeout_exit_and_stop(self):
  cfg=self.load();cfg['servers'][1]['enabled']=True;made=[];events=[]
  def pop(*a,**k):
   events.append('p'+a[0][a[0].index('--port')+1]);f=Fake(*a,**k);made.append(f);return f
  l=multi.Launcher(cfg,pop,lambda _:None,lambda p:events.append('r'+str(p)) or True,iter([0,1,2,3]).__next__,lambda _:None);l.start()
  self.assertLess(events.index('r19002'),events.index('p19003'));l.cleanup()
  cfg=self.load();cfg['servers'][1]['enabled']=True;made=[]
  events=[]
  class Ordered(Fake):
   def wait(self, timeout=None): events.append('wait-a'); return super().wait(timeout)
  def ordered_popen(*a, **k): events.append('popen-'+a[0][a[0].index('--port')+1]); f=Ordered(*a, **k); made.append(f); return f
  l=multi.Launcher(cfg, ordered_popen, lambda _:None, lambda p:p==19003, iter([0,20,30,31]).__next__, lambda _:None);l.start()
  self.assertEqual(len(made),2);self.assertLess(events.index('wait-a'), events.index('popen-19003'));self.assertTrue(made[0].terminated and made[0].killed);l.cleanup()
  cfg=self.load();cfg['servers'][1]['enabled']=True;made=[]
  def exitpop(*a,**k):f=Fake(*a,**k);f.dead=True;made.append(f);return f
  events=[]
  def exitpop(*a,**k):
   f=Fake(*a,**k); f.dead=True; oldwait=f.wait
   f.wait=lambda timeout=None: (events.append('wait-exit'), oldwait(timeout))[1]
   events.append('popen-'+a[0][a[0].index('--port')+1]); made.append(f); return f
  l=multi.Launcher(cfg,exitpop,lambda _:None,lambda _:True,iter([0,1,2,3]).__next__,lambda _:None);l.start();self.assertLess(events.index('wait-exit'),events.index('popen-19003'));l.cleanup()
  cfg=self.load();f=Fake(multi.server_argv(cfg,cfg['servers'][0]),cfg['servers'][0]['cwd'])
  child=multi.Child(cfg,cfg['servers'][0],lambda *_a,**_k:f,lambda _:False,lambda:0,lambda _:None);child.wait_ready(lambda:True)
  self.assertTrue(child.reaped and f.terminated and f.killed)

 def _test_landing_payload_host_and_no_browser_on_primary_failure(self):
  port=free();cfg=self.load(landing_port=port,open_browser=True);made=[];opened=[];old=multi.STARTUP_SECONDS;multi.STARTUP_SECONDS=.001
  try:
   l=multi.Launcher(cfg,lambda *a,**k:made.append(Fake(*a,**k)) or made[-1],opened.append,lambda _:False,iter([0,20,30]).__next__,lambda _:None);l.start();self.assertEqual(opened,[]);self.assertEqual(([] if l.import_url() is None else json.loads(__import__('urllib').parse.unquote(l.import_url().split('#eagent-workspaces=', 1)[1]))['workspaces']),[]);l.cleanup()
   cfg=self.load(landing_port=port,open_browser=False);l=multi.Launcher(cfg,lambda *a,**k:Fake(*a,**k),lambda _:None,lambda p:p==19002,iter([0,1,2]).__next__,lambda _:None);l.start();r=urlopen('http://127.0.0.1:%d/'%port);body=r.read().decode();self.assertEqual(r.headers['Cache-Control'],'no-store');self.assertNotIn('Access-Control-Allow-Origin',r.headers);self.assertNotIn('cwd',body);self.assertNotIn('profile',body);self.assertNotIn('token',body)
   for u in ('/x','/?x'):
    with self.assertRaises(HTTPError):urlopen('http://127.0.0.1:%d%s'%(port,u))
   with self.assertRaises(HTTPError):urlopen(Request('http://127.0.0.1:%d/'%port,headers={'Host':'localhost:%d'%port}))
   l.cleanup()
  finally:multi.STARTUP_SECONDS=old
 def _test_linux_fake_e2e_two_ready_one_failed(self):
  lp,a,b,c=free(),free(),free(),free();self.assertNotIn(15403,(lp,a,b,c));fake=self.r/'fake';fake.write_text('#!/usr/bin/env python3\nimport sys\nfrom http.server import HTTPServer,BaseHTTPRequestHandler\nif "--profile" in sys.argv and sys.argv[sys.argv.index("--profile")+1]=="fail":raise SystemExit(3)\nclass H(BaseHTTPRequestHandler):\n def do_GET(self):self.send_response(200);self.end_headers()\n def log_message(self,*x):pass\nHTTPServer(("127.0.0.1",int(sys.argv[sys.argv.index("--port")+1])),H).serve_forever()');fake.chmod(0o755)
  ss=[{'id':'a','label':'&<A>','cwd':'a dir','port':a,'enabled':True,'args':[]},{'id':'b','label':'B','cwd':'b','port':b,'enabled':True,'args':[]},{'id':'c','label':'C','cwd':'b','port':c,'enabled':True,'profile':'fail','args':[]}]
  # duplicate enabled cwd is schema-invalid; use a third directory for the failed record.
  (self.r/'c').mkdir();ss[2]['cwd']='c';cfg=self.load(command=str(fake),landing_port=lp,open_browser=True,servers=ss);opened=[];l=multi.Launcher(cfg,browser_open=opened.append);l.start()
  try:self.assertEqual([x['id'] for x in ([] if l.import_url() is None else json.loads(__import__('urllib').parse.unquote(l.import_url().split('#eagent-workspaces=', 1)[1]))['workspaces'])],['a','b']);self.assertEqual(l.children[2].status,'failed');self.assertIn('&amp;&lt;A&gt;',urlopen('http://127.0.0.1:%d/'%lp).read().decode());self.assertEqual(len(opened),1)
  finally:l.cleanup()
  self.assertTrue(all(x.reaped for x in l.children))
class SecurityTests(Base, unittest.TestCase):
    def test_validation_matrix(self): self._test_validation_matrix()
    def test_sequential_ready_timeout_exit_and_stop(self): self._test_sequential_ready_timeout_exit_and_stop()
    def test_landing_payload_host_and_no_browser_on_primary_failure(self): self._test_landing_payload_host_and_no_browser_on_primary_failure()
    def test_linux_fake_e2e_two_ready_one_failed(self): self._test_linux_fake_e2e_two_ready_one_failed()

class ReadinessAndRunTests(Base, unittest.TestCase):
    def test_ready_at_refuses_redirect_destination_and_proxy(self):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        import threading
        destination_hits, proxy_hits = [], []
        class Destination(BaseHTTPRequestHandler):
            def do_GET(self): destination_hits.append(self.path); self.send_response(200); self.end_headers()
            def log_message(self, *_): pass
        destination = ThreadingHTTPServer(('127.0.0.1', free()), Destination)
        class Source(BaseHTTPRequestHandler):
            code = 200
            def do_GET(self):
                self.send_response(self.code)
                if self.code == 302: self.send_header('Location', 'http://127.0.0.1:%d/' % destination.server_port)
                self.end_headers()
            def log_message(self, *_): pass
        source = ThreadingHTTPServer(('127.0.0.1', free()), Source)
        class Proxy(BaseHTTPRequestHandler):
            def do_GET(self): proxy_hits.append(self.path); self.send_response(200); self.end_headers()
            def log_message(self, *_): pass
        proxy = ThreadingHTTPServer(('127.0.0.1', free()), Proxy)
        for server in (destination, source, proxy): threading.Thread(target=server.serve_forever, daemon=True).start()
        old = {key: os.environ.get(key) for key in ('http_proxy', 'HTTP_PROXY', 'no_proxy', 'NO_PROXY')}
        try:
            os.environ['http_proxy'] = os.environ['HTTP_PROXY'] = 'http://127.0.0.1:%d' % proxy.server_port
            os.environ['no_proxy'] = os.environ['NO_PROXY'] = ''
            # Build a fresh opener after env setup, like module initialization.
            opener = multi.urllib.request.build_opener(multi.urllib.request.ProxyHandler({}), multi._NoRedirect())
            def probe(port):
                try:
                    with opener.open('http://127.0.0.1:%d/' % port, timeout=.25) as response:
                        return 200 <= response.status < 300
                except (OSError, multi.urllib.error.HTTPError): return False
            for code, expected in ((200, True), (201, True), (204, True), (401, False), (404, False), (500, False)):
                Source.code = code; self.assertEqual(probe(source.server_port), expected)
            Source.code = 302
            self.assertFalse(probe(source.server_port))
            self.assertEqual(destination_hits, [])
            self.assertFalse(probe(free()))
            self.assertEqual(proxy_hits, [])
            # Negative control: without ProxyHandler({}), this unavailable
            # loopback target is answered by the synthetic proxy instead.
            unsafe = multi.urllib.request.build_opener(multi._NoRedirect())
            with unsafe.open('http://127.0.0.1:%d/' % free(), timeout=.25) as response:
                self.assertEqual(response.status, 200)
            self.assertEqual(len(proxy_hits), 1)
        finally:
            for key, value in old.items():
                if value is None: os.environ.pop(key, None)
                else: os.environ[key] = value
            for server in (destination, source, proxy): server.shutdown(); server.server_close()

    def test_exact_popen_and_run_cleanup(self):
        cfg = self.load(); created = []
        def popen(*args, **kwargs):
            child = Fake(*args, **kwargs); created.append(child); return child
        child = multi.Child(cfg, cfg['servers'][0], popen)
        self.assertEqual(child.proc.argv, [str(self.exe.resolve()), 'web', '--host', '127.0.0.1',
            '--port', '19002', '--workspace', str((self.r / 'a dir').resolve()), '--profile', 'p', '--read-only'])
        self.assertEqual(child.proc.cwd, str((self.r / 'a dir').resolve()))
        self.assertFalse(child.proc.kw['shell'])
        self.assertEqual(tuple(child.proc.kw[k] for k in ('stdin', 'stdout', 'stderr')), (subprocess.DEVNULL,) * 3)
        child.stop()

        port = free(); cfg = self.load(landing_port=port); made = []
        # A KeyboardInterrupt while start is waiting must still close landing and reap.
        launcher = multi.Launcher(cfg, lambda *a, **k: made.append(Fake(*a, **k)) or made[-1],
            probe=lambda _: (_ for _ in ()).throw(KeyboardInterrupt()), clock=lambda: 0, pause=lambda _: None)
        launcher.run()
        self.assertTrue(all(child.reaped for child in launcher.children))
        self.assertTrue(multi.port_is_free(port))

        port = free(); cfg = self.load(landing_port=port); made = []
        class FailingStart(multi.Launcher):
            def start(self):
                super().start()
                raise RuntimeError('after children')
        launcher = FailingStart(cfg, lambda *a, **k: made.append(Fake(*a, **k)) or made[-1],
            probe=lambda _: True, clock=lambda: 0, pause=lambda _: None)
        with self.assertRaisesRegex(RuntimeError, 'after children'): launcher.run()
        self.assertTrue(all(child.reaped for child in launcher.children))
        self.assertTrue(multi.port_is_free(port))

    def test_primary_exit_before_browser_is_not_imported(self):
        cfg = self.load(open_browser=True); cfg['servers'][1]['enabled'] = True
        made, opened = [], []
        def popen(*args, **kwargs):
            f = Fake(*args, **kwargs); made.append(f); return f
        def probe(port):
            if port == 19003: made[0].dead = True
            return True
        launcher = multi.Launcher(cfg, popen, opened.append, probe, iter([0, 1, 2, 3]).__next__, lambda _: None)
        launcher.start()
        try:
            self.assertEqual([w['id'] for w in json.loads(__import__('urllib').parse.unquote(launcher.import_url('b').split('#eagent-workspaces=', 1)[1]))['workspaces']], ['b'])
            self.assertEqual(opened, [])
            self.assertTrue(launcher.children[0].reaped)
        finally:
            launcher.cleanup()


class EntrypointAndBoundsTests(Base, unittest.TestCase):
    def _many(self, width):
        servers = []
        for i in range(32):
            directory = self.r / ('w%d' % i); directory.mkdir(exist_ok=True)
            servers.append({'id': 'w%d' % i, 'label': '😀' * width, 'cwd': directory.name,
                            'port': 21000 + i, 'enabled': True, 'args': []})
        return servers

    def test_utf8_payload_bound_before_spawn(self):
        # 90 emoji per label is valid in Python and the actual GJS parser.
        config = self.load(primary='w0', servers=self._many(90))
        raw = multi._payload_bytes(config['landing_port'], config['primary'],
                                   [server for server in config['servers'] if server['enabled']])
        self.assertLessEqual(len(raw), multi.MAX_PAYLOAD_BYTES)
        env = dict(os.environ, MODE='fragment', CROSS_PAYLOAD=raw.decode('utf-8'))
        harness = Path(__file__).parents[1] / 'src/ui/test_harness.py'
        result = subprocess.run([os.sys.executable, str(harness)], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('Python UTF-8 payload parses with codepoint names', result.stdout)
        with self.assertRaises(multi.ConfigError):
            self.load(primary='w0', servers=self._many(121))

    def test_landing_ready_links_and_failed_primary_alternate(self):
        landing, a, b = free(), free(), free()
        cfg = self.load(landing_port=landing, open_browser=True, primary='a', servers=[
            {'id': 'a', 'label': 'failed <A>', 'cwd': 'a dir', 'port': a, 'enabled': True, 'args': []},
            {'id': 'b', 'label': 'ready &B', 'cwd': 'b', 'port': b, 'enabled': True, 'args': []},
        ])
        made, opened = [], []
        def popen(*args, **kwargs):
            child = Fake(*args, **kwargs); made.append(child); return child
        launcher = multi.Launcher(cfg, popen, opened.append,
            probe=lambda port: port == b, clock=iter([0, 20, 21, 22]).__next__, pause=lambda _: None)
        launcher.start()
        try:
            body = urlopen('http://127.0.0.1:%d/' % landing).read().decode()
            self.assertIn('failed &lt;A&gt;: failed', body)
            self.assertIn('ready &amp;B: ready', body)
            self.assertEqual(opened, [])
            self.assertEqual(body.count('<a href='), 1)
            href = body.split('<a href="', 1)[1].split('"', 1)[0]
            from urllib.parse import unquote
            payload = json.loads(unquote(href.split('#eagent-workspaces=', 1)[1]))
            self.assertEqual(payload['primary'], 'b')
            self.assertEqual([item['id'] for item in payload['workspaces']], ['b'])
            self.assertNotIn('token', href)
        finally:
            launcher.cleanup()

    def test_cleanup_reaps_raced_child_and_continues(self):
        cfg = self.load(); launcher = multi.Launcher(cfg)
        first = Fake([], '')
        second = Fake([], '')
        class Race:
            def __init__(self, proc): self.proc, self.reaped = proc, False
            def stop(self): raise OSError('race')
            def _reap_exited(self): self.proc.wait(); self.reaped = True
        first.dead = True
        launcher.children = [Race(first), Race(second)]
        launcher.cleanup()
        self.assertTrue(launcher.children[0].reaped)
        self.assertFalse(launcher.children[1].reaped)

class PortAndSnapshotTests(Base, unittest.TestCase):
    def test_port_80_rejected_for_landing_and_disabled_child(self):
        with self.assertRaises(multi.ConfigError): self.load(landing_port=80)
        servers = self.raw()['servers']; servers[1]['port'] = 80
        with self.assertRaises(multi.ConfigError): self.load(servers=servers)

    def test_alternate_primary_bound_before_spawn(self):
        directories = []
        servers = []
        for i in range(32):
            directory = self.r / ('p%d' % i); directory.mkdir(); directories.append(directory)
            # Tune a valid 120-codepoint label set so the 63-byte primary-ID
            # difference crosses the 16 KiB boundary.
            label = '😀' * 113 + ('x' * (2 if i == 0 else 7) if i < 10 else '')
            servers.append({'id': 'a' if i == 0 else ('z' * 64 if i == 1 else 'x%d' % i),
                            'label': label, 'cwd': directory.name, 'port': 22000 + i,
                            'enabled': True, 'args': []})
        # The configured one-char primary fits where the longest alternate does not.
        short = multi._payload_bytes(19001, 'a', servers)
        long = multi._payload_bytes(19001, 'z' * 64, servers)
        self.assertLessEqual(len(short), multi.MAX_PAYLOAD_BYTES)
        self.assertGreater(len(long), multi.MAX_PAYLOAD_BYTES)
        path = self.conf(primary='a', servers=servers)
        with self.assertRaises(multi.ConfigError): multi.load_config(path)

    def test_import_url_uses_one_ready_snapshot(self):
        cfg = self.load(); cfg['servers'][1]['enabled'] = True
        launcher = multi.Launcher(cfg)
        first = type('Child', (), {'spec': cfg['servers'][0], 'status': 'ready'})()
        second = type('Child', (), {'spec': cfg['servers'][1], 'status': 'ready'})()
        launcher.children = [first, second]
        original = launcher.children
        url = launcher.import_url('a')
        self.assertIs(launcher.children, original)
        payload = json.loads(__import__('urllib').parse.unquote(url.split('#eagent-workspaces=', 1)[1]))
        self.assertEqual(payload['primary'], 'a')
        self.assertEqual([w['id'] for w in payload['workspaces']], ['a', 'b'])

if __name__ == '__main__':
    unittest.main()
