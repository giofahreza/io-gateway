#!/usr/bin/env python3
"""Synthetic Qwen provider management checks. Never calls a provider endpoint.

Run after cargo build --locked --bins. Console transport itself is tested by
target::compatible::quota::tests against an in-process HTTP fixture.
Pass --serve for browser/TUI inspection of accounts without a console session.
"""
import argparse
import importlib.util
import json
import sys
from pathlib import Path
import time

ROOT = Path(__file__).resolve().parents[1]
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('compatible_fixture', ROOT/'scripts/test-compatible-apis.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
request, check = fixture.request, fixture.check

def run(serve):
    binary = ROOT/'target/debug/io-gateway'
    with fixture.gateway(binary, auth_enabled=True) as (base, _):
        check(request(base,'/compatible/quota.json')[0] == 401, 'Qwen quota requires admin authentication')
        for endpoint, body in [('connect',{'file_name':'plan.json'}),('status',{'login_id':'login'}),
                               ('cancel',{'login_id':'login'}),('disconnect',{'file_name':'plan.json'})]:
            check(request(base,'/compatible/qwen/'+endpoint,body)[0] == 401, 'Qwen '+endpoint+' requires admin authentication')
    saved_account = {'type':'compatible','account_id':'connected-fixture','label':'Connected fixture',
        'service':'qwen_token_plan','api_key':'sk-sp-private-fixture','models':['auto'],
        'cloud_auth':{'access_token':'synthetic-private-management-token','expires_at':'2000-01-01T00:00:00Z',
            'email':'owner@example.test','user_id':'private-user-id'}}
    with fixture.gateway(binary, initial_accounts={'connected.json':saved_account}) as (base, directory):
        _, listing = request(base,'/compatible/accounts.json')
        connected = listing['accounts'][0]
        check(connected['quota_connection']['status']=='expired' and connected['quota_connection']['email']=='owner@example.test', 'connection metadata reports expiry and identity')
        _, quota = request(base,'/compatible/quota.json')
        check(quota['accounts'][0]['quota_status']=='login_required' and 'expired' in quota['accounts'][0]['notes'][0], 'expired connection requests reconnect without an upstream read')
        for secret in ['synthetic-private-management-token','sk-sp-private-fixture','private-user-id']:
            check(secret not in json.dumps([listing,quota]), 'management credentials stay private')
        edit = {'file_name':'connected.json','label':'Renamed','service':'qwen_token_plan','models':['auto']}
        check(request(base,'/compatible/save',edit)[0]==200, 'edit connected account')
        path = directory/'auths'/'connected.json'
        check(json.loads(path.read_text())['cloud_auth']==saved_account['cloud_auth'], 'editing preserves management login')
        check(request(base,'/compatible/qwen/disconnect',{'file_name':'connected.json'})[0]==200, 'disconnect expired connection')
        retained = json.loads(path.read_text())
        check('cloud_auth' not in retained and retained['api_key']==saved_account['api_key'], 'disconnect removes management token and preserves inference')
        _, quota = request(base,'/compatible/quota.json')
        check('expired' not in quota['accounts'][0]['notes'][0], 'disconnect clears cached quota')
    with fixture.gateway(binary, initial_accounts={'connected.json':saved_account}) as (base,directory):
        check(request(base,'/compatible/save',{**edit,'api_key':'sk-sp-replacement-fixture'})[0]==200, 'replace inference key')
        check(json.loads((directory/'auths'/'connected.json').read_text())['cloud_auth'] is None, 'replacing API key clears previous management account')
    with fixture.gateway(binary) as (base, directory):
        files = []
        for service, host, key in [
            ('qwen_token_plan','token-plan','sk-sp-synthetic-private-plan-key'),
            ('qwen_payg','ap-southeast-1','sk-ws-synthetic-private-payg-key')]:
            body = {'service':service,'label':service,'api_key':key,'models':['auto'],
                'base_url':f'https://{host}.maas.qwencloudapi.com/compatible-mode/v1',
                'anthropic_base_url':f'https://{host}.maas.qwencloudapi.com/apps/anthropic'}
            status, saved = request(base,'/compatible/save',body)
            check(status == 200, 'save '+service)
            file = saved['file_name']; files.append(file)
            status, result = request(base,'/compatible/accounts.json')
            account = next(a for a in result['accounts'] if a['file_name'] == file)
            check(account['service'] == service and account['display_provider'] == 'qwen' and not account['has_console_cookie'], 'Qwen group metadata '+service)
            check(key not in json.dumps(result), 'API key stays private')
            check(account['quota_connection']['status'] == 'disconnected', 'quota sign-in is optional '+service)
            status, quotas = request(base,'/compatible/quota.json')
            quota = next(q for q in quotas['accounts'] if q['file_name'] == file)
            check(status == 200 and quota['credits']['balance'] is None and quota['limits'] == [], 'missing session never becomes a zero balance or invented quota')
            check('inference API key' in quota['notes'][0] and quota['quota_status'] == 'login_required', 'missing-session setup is explained')
            check(request(base,'/compatible/save',{**body,'api_key':None,'file_name':file})[0] == 200,'editing preserves saved key and account identity')
            wrong = 'sk-ws-wrong' if service == 'qwen_token_plan' else 'sk-sp-wrong'
            check(request(base,'/compatible/save',{**body,'api_key':wrong})[0] == 400, 'reject mixed plan/payg keys')
            check(request(base,'/compatible/save',{**body,'console_cookie':'invalid\nheader'})[0] == 400,'reject malformed console session')
            check(request(base,'/compatible/qwen/disconnect',{'file_name':file})[0] == 200, 'disconnect quota without deleting API configuration')
            _, listing = request(base,'/compatible/accounts.json')
            retained = next(a for a in listing['accounts'] if a['file_name'] == file)
            check(retained['has_api_key'] and retained['models'] == ['auto'], 'disconnect preserves API key and models')
        for endpoint, body in [('connect',{'file_name':'../../config.json'}),('disconnect',{'file_name':'../../config.json'}),('status',{'login_id':'unknown'})]:
            check(request(base,'/compatible/qwen/'+endpoint,body)[0] == 400, 'reject invalid quota login target '+endpoint)
        for body in [
            {'service':'qwen_token_plan','label':'No URLs','api_key':'sk-sp-synthetic-defaults'},
            {'service':'qwen_payg','label':'Payg defaults','api_key':'sk-ws-synthetic-defaults','models':['test-model']},
            {'service':'custom','label':'Detected Token Plan','api_key':'sk-sp-synthetic-detection','base_url':'https://token-plan.maas.qwencloudapi.com/compatible-mode/v1','models':['auto']},
        ]:
            status,saved=request(base,'/compatible/save',body)
            check(status == 200,'Qwen defaults / URL detection: '+body['label'])
            _,result=request(base,'/compatible/accounts.json')
            account=next(a for a in result['accounts'] if a['file_name']==saved['file_name'])
            check(account['display_provider']=='qwen' and account['service']!='custom','known Qwen URLs never stay in custom group')
            if body['service']!='custom':
                check(account['base_url'].endswith('/compatible-mode/v1') and account['anthropic_base_url'].endswith('/apps/anthropic'),'provider fills both protocol endpoints')
            if body['service']=='qwen_token_plan':check(account['models']==['auto'],'Token Plan defaults to auto model')
            request(base,'/credentials/delete',form={'file_name':saved['file_name']})
        request(base,'/compatible/quota.json')
        status, snapshot = request(base,'/dashboard/snapshot.json')
        check(status == 200 and len(snapshot['quotas']['compatible']['accounts']) == 2,'shared snapshot contains Qwen quotas')
        for file in files:
            saved = json.loads((directory/'auths'/file).read_text())
            check((directory/'auths'/file).stat().st_mode & 0o777 == 0o600,'credential file is private')
            assert saved.get('console_cookie') is None
        if serve:
            print('BROWSER_GATEWAY='+base,flush=True)
            try:
                while True:time.sleep(1)
            except KeyboardInterrupt:pass

if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--serve',action='store_true')
    run(parser.parse_args().serve)
