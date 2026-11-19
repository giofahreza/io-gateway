#!/usr/bin/env python3
"""Synthetic-only reset integration. NEVER reads real credentials or calls a provider.

cargo build --locked --bins && python3 scripts/test-claude-resets.py
Pass --serve for browser/TUI inspection of the synthetic account.
"""
import argparse
import datetime
import sqlite3
import concurrent.futures
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT=Path(__file__).resolve().parents[1]
CALLS=[]
MODE='normal'
LEFT=1
EXPIRY=(datetime.datetime.now(datetime.timezone.utc)+datetime.timedelta(minutes=45)).isoformat()

def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1',0))
        return s.getsockname()[1]

def usage():
    automatic=MODE.startswith('auto')
    reset=(datetime.datetime.now(datetime.timezone.utc)+datetime.timedelta(minutes=2 if MODE=='auto-near' else 90)).isoformat()
    return {'five_hour':{'utilization':100 if automatic else 75,'resets_at':reset if automatic else '2099-01-01T00:00:00Z'},
            'seven_day':{'utilization':90,'resets_at':'2099-01-02T00:00:00Z'},
            'spend':{'enabled':True,'balance':{'amount_minor':1250,'currency':'USD','exponent':2},
                     'used':{'amount_minor':250,'currency':'USD','exponent':2},
                     'limit':{'amount_minor':2000,'currency':'USD','exponent':2}},
            'cedar_ember':{'eligible':MODE!='ineligible','at_limit':automatic,'grants':[{
                'id':'full-reset-test','label':'Full limit reset','resets_left':LEFT,'resets_total':1,
                'starts_at':'2020-01-01T00:00:00Z','ends_at':EXPIRY if automatic or MODE=='scheduled-idle' else '2099-01-01T00:00:00Z',
                'paused':MODE=='paused','usable_now':True,'use_requires_limit':False,
                'clears':['five_hour','seven_day'],'type':'full'}]}}

class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def reply(self,value,status=200):
        body=json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type','application/json')
        self.send_header('Content-Length',str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        CALLS.append(('GET',self.path))
        if self.path.startswith('/api/oauth/usage'):
            assert 'cedar_ember=1' in self.path and 'at_wall=1' in self.path
            self.reply(usage())
        elif self.path.endswith('/usage'):
            self.reply({'credits':{'has_credits':True,'unlimited':False,'balance':'8.25'}})
        elif self.path.endswith('/rate-limit-reset-credits'):
            self.reply({'available_count':0,'credits':[]})
        elif '/models' in self.path: self.reply({'models':[],'data':[]})
        else: self.reply({},404)
    def do_POST(self):
        global LEFT
        body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        CALLS.append(('POST',self.path,body))
        assert self.path=='/api/organizations/org-test/reset_rate_limits'
        assert body['program']=='cedar_ember' and body['grant_id']=='full-reset-test'
        assert body['request_id']
        if MODE.endswith('ambiguous'):
            self.reply({'unexpected':'response'})
        else:
            LEFT=0
            self.reply({'result':'reset','resets_left':0,'cleared':['five_hour','seven_day']})

def request(base,path,form=None,json_body=None):
    data=json.dumps(json_body).encode() if json_body is not None else None if form is None else urllib.parse.urlencode(form).encode()
    content_type='application/json' if json_body is not None else 'application/x-www-form-urlencoded'
    r=urllib.request.Request(base+path,data=data,headers={'Content-Type':content_type})
    try: response=urllib.request.urlopen(r,timeout=30)
    except urllib.error.HTTPError as e: response=e
    text=response.read().decode()
    try:value=json.loads(text)
    except ValueError:value=text
    return response.status,value

def run():
    global MODE,LEFT
    parser=argparse.ArgumentParser()
    parser.add_argument('--serve',action='store_true')
    parser.add_argument('--automation',action='store_true')
    args=parser.parse_args()
    upstream=http.server.ThreadingHTTPServer(('127.0.0.1',port()),Upstream)
    threading.Thread(target=upstream.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='iogw-claude-reset-') as tmp:
        directory=Path(tmp)
        auth=directory/'auths'; auth.mkdir(mode=0o700)
        upstream_base=f'http://127.0.0.1:{upstream.server_port}'
        (auth/'claude.json').write_text(json.dumps({'type':'claude','access_token':'synthetic-claude-token',
            'organization_uuid':'org-test','account_id':'account-test','label':'Claude reset fixture',
            'api_base_url':upstream_base,'expires_at':'2099-01-01T00:00:00Z'}))
        (auth/'codex.json').write_text(json.dumps({'type':'codex','access_token':'synthetic-codex-token','account_id':'codex-test','label':'Codex credit fixture'}))
        cfg={'listen':f'127.0.0.1:{port()}','tokens':[],'auth_dir':str(auth),'proxy_api_key':'synthetic-gateway-key',
             'upstream_base':upstream_base+'/codex','admin_auth':{'enabled':False}}
        config=directory/'config.json'; config.write_text(json.dumps(cfg))
        base='http://'+cfg['listen']
        binary=Path(os.environ.get('IOGW_TEST_BINARY',ROOT/'target/debug/io-gateway'))
        proc=None
        log=(directory/'gateway.log').open('w+')
        def start():
            nonlocal proc
            env={k:v for k,v in os.environ.items() if not k.startswith(('ADMIN_AUTH_','CODEX_GATEWAY_KEY'))}
            proc=subprocess.Popen([str(binary),'--config',str(config)],stdout=log,stderr=log,cwd=directory,env=env)
            for _ in range(300):
                try:
                    status,_=request(base,'/health')
                    if status==200:return
                except Exception: pass
                if proc.poll() is not None:
                    log.seek(0); raise AssertionError(log.read())
                time.sleep(.1)
            log.seek(0); raise AssertionError('Gateway did not start: '+log.read())
        def stop():
            proc.terminate();proc.wait(timeout=10)
        def refresh():
            assert request(base,'/models/refresh',{'provider':'claude'})[0]==200
            return request(base,'/claude/quota.json')[1]['accounts'][0]
        form={'file_name':'claude.json','credit_id':'full-reset-test','program':'cedar_ember'}
        path='/claude/rate-limit-reset-credit/consume'
        posts=lambda:len([c for c in CALLS if c[0]=='POST'])
        try:
            start()
            if args.automation:
                endpoint='/admin/claude/reset-credit-automation'
                policy={'account_key':'claude:organization:org-test:account:account-test','enabled':False,'scan_interval_minutes':30,'expiry_window_minutes':60,'final_attempt_minutes':5,'min_natural_reset_remaining_minutes':10}
                ledger=auth/'claude-reset-actions.sqlite3'
                assert request(base,endpoint)[1]['policies']==[] and not ledger.exists()
                assert request(base,endpoint,json_body={**policy,'scan_interval_minutes':0})[0]==400
                assert request(base,endpoint,json_body=policy)[0]==200
                stop();MODE='auto';start()
                time.sleep(.5)
                assert posts()==0
                # All test ledger operations below target this temporary synthetic account.
                def rows():
                    with sqlite3.connect(ledger) as db:return db.execute('SELECT state FROM claude_reset_actions').fetchall()
                def wait_state(expected):
                    for _ in range(150):
                        if any(r[0]==expected for r in rows()):return
                        time.sleep(.1)
                    raise AssertionError((expected,rows(),CALLS))
                def reset_fixture(mode):
                    global MODE,LEFT
                    stop()
                    with sqlite3.connect(ledger) as db:
                        db.execute('DELETE FROM claude_reset_actions')
                        db.execute('UPDATE claude_reset_policies SET next_scan_at=0')
                    MODE=mode;LEFT=1;start()
                assert request(base,endpoint,json_body={**policy,'enabled':True})[0]==200
                reset_fixture('scheduled-idle');wait_state('deferred');assert posts()==0
                with sqlite3.connect(ledger) as db:
                    deadline=db.execute('SELECT next_attempt_at,credit_expires_at FROM claude_reset_actions').fetchone()
                    assert deadline[0] <= deadline[1]-5*60
                assert request(base,endpoint,json_body=policy)[0]==200
                assert rows()==[('cancelled',)]
                # Re-enabling can reclaim an unsubmitted cancelled grant.
                stop();MODE='auto';assert posts()==0
                start();assert request(base,endpoint,json_body={**policy,'enabled':True})[0]==200
                stop();start();wait_state('verified');assert posts()==1
                # Completed entitlements never replay even with stale upstream status.
                stop();LEFT=1
                with sqlite3.connect(ledger) as db:db.execute('UPDATE claude_reset_policies SET next_scan_at=0')
                start();time.sleep(.5);assert posts()==1
                reset_fixture('auto-near');wait_state('deferred');assert posts()==1
                assert request(base,endpoint,json_body=policy)[0]==200
                assert rows()==[('cancelled',)] and posts()==1
                assert request(base,endpoint,json_body={**policy,'enabled':True})[0]==200
                reset_fixture('auto-ambiguous');wait_state('manual_review');assert posts()==2
                stop();LEFT=2
                with sqlite3.connect(ledger) as db:db.execute('UPDATE claude_reset_policies SET next_scan_at=0')
                start();time.sleep(.5);assert posts()==2
                # A crash after submission cannot replay the request.
                stop()
                with sqlite3.connect(ledger) as db:
                    db.execute("UPDATE claude_reset_actions SET state='submitted',lease_until=1")
                    db.execute('UPDATE claude_reset_policies SET next_scan_at=0')
                start();wait_state('manual_review');assert posts()==2
                assert request(base,endpoint,json_body=policy)[0]==200
                assert refresh()['reset_automation']['enabled'] is False
                assert request(base,'/usage/mobile.json')[1]['quotas']['claude']['accounts'][0]['reset_automation']['scan_interval_minutes']==30
                stop();cfg['admin_auth']={'enabled':True,'api_key':'synthetic-admin-key'};config.write_text(json.dumps(cfg));start()
                assert request(base,endpoint)[0]==401
                assert request(base,endpoint,json_body=policy)[0]==401
                print('Claude automation passed: opt-in defaults, validation, deferral, final-check deadline, cancellation, re-enable, one exact grant, natural reset avoidance, ambiguous/crash lock, restart durability, mobile policy and admin auth. Synthetic credentials only.')
                return
            quota=refresh()
            assert quota['credits']['balance']=='12.50'
            assert quota['credits']['used']=='2.50'
            assert quota['rate_limit_reset_credits']['available_count']==1
            assert quota['rate_limit_reset_credits']['credits'][0]['usable_now'] is True
            mobile=request(base,'/usage/mobile.json')[1]
            # The compact mobile projection retains balances and grant eligibility.
            account=mobile['quotas']['claude']['accounts'][0]
            assert account['credits']['balance']=='12.50'
            assert account['rate_limit_reset_credits']['credits'][0]['program']=='cedar_ember'
            assert account['rate_limit_reset_credits']['credits'][0]['usable_now'] is True
            assert posts()==0
            if args.serve:
                print(json.dumps({'base_url':base,'auth_dir':str(auth)}),flush=True)
                while True:time.sleep(1)
            for mode in ['paused','ineligible']:
                MODE=mode
                assert request(base,path,form)[0]==409
                assert posts()==0
            MODE='normal'
            assert request(base,path,{**form,'credit_id':'not-a-grant'})[0]==409
            assert request(base,path,{**form,'file_name':'missing.json'})[0]==409
            with concurrent.futures.ThreadPoolExecutor(2) as pool:
                results=list(pool.map(lambda _:request(base,path,form),range(2)))
            assert posts()==1, CALLS
            assert any(value.get('outcome')=='verified' for _,value in results)
            assert request(base,path,form)[0]==409 and posts()==1
            stop()
            # Restarts cannot replay a completed grant, even if upstream returns stale eligibility.
            LEFT=1; start()
            assert request(base,path,form)[0]==409 and posts()==1
            # Use a new synthetic entitlement slot to test ambiguous transport results.
            LEFT=2; MODE='ambiguous'
            assert request(base,path,form)[1]['outcome']=='manual_review'
            assert posts()==2
            assert refresh()['last_reset_action']['state']=='manual_review'
            stop();start()
            assert request(base,path,form)[0]==409 and posts()==2
            stop()
            cfg['admin_auth']={'enabled':True,'api_key':'synthetic-admin-key'}
            config.write_text(json.dumps(cfg));start()
            assert request(base,path,form)[0]==401 and posts()==2
            print('Claude reset integration passed: balances, mobile contract, fresh eligibility, concurrent clicks, restart durability, ambiguous results, admin authentication. Synthetic credentials only.')
        finally:
            if proc and proc.poll() is None:stop()
            log.close()
            upstream.shutdown()

if __name__=='__main__':run()
