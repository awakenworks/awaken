"""Native's generic recipient envelope, consumed only in the private executor.

Wire-compatible with the independent JS consumer. No Vault or model tool lives
here. HTTP errors and cryptographic diagnostics never include material.
"""
import base64
import json
from pathlib import Path
from urllib.parse import urlparse
import time
import httpx
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
class CredentialDeliveryError(Exception):
    def __init__(self,*_args): super().__init__("credential_delivery_unavailable")


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':'))

class CredentialClient:
    def __init__(self, url, token_file, recipient_file, consumer, material_type, handling_mode):
        parsed = urlparse(url)
        if parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in ('', '/') or not (
            parsed.scheme == 'https' or parsed.scheme == 'http' and parsed.hostname in ('localhost', '127.0.0.1', '::1')):
            raise CredentialDeliveryError('CREDENTIAL_DELIVERY_UNAVAILABLE', 503)
        if not material_type or handling_mode not in ('platform_managed','application_managed'): raise CredentialDeliveryError()
        self.material_type,self.handling_mode=material_type,handling_mode
        self.url, self.token_file, self.recipient_file, self.consumer = url.rstrip('/'), Path(token_file), Path(recipient_file), consumer

    def _call(self, route, body):
        try:
            with httpx.Client(timeout=15, follow_redirects=False, trust_env=False) as client:
                r = client.post(self.url+'/v1/config/application-credentials/'+route, json=body,
                    headers={'Authorization': 'Bearer '+self.token_file.read_text().strip()})
                if r.status_code != 200: raise ValueError()
                return r.json()
        except Exception:
            raise CredentialDeliveryError('CREDENTIAL_DELIVERY_UNAVAILABLE', 503) from None

    def enter(self, account, fields, revision=None):
        command = dict(consumer_id=self.consumer, credential_key=account, material_type=self.material_type, fields=fields)
        if revision: command['expected_revision'] = revision
        return self._call('enter', command)['credential_ref']

    def request(self, binding):
        keys = json.loads(self.recipient_file.read_text())['keys']
        if len(keys) != 1: raise CredentialDeliveryError('CREDENTIAL_DELIVERY_UNAVAILABLE', 503)
        return dict(consumer_id=self.consumer, credential_key=binding['account_ref'], material_type=self.material_type,
            expected_revision=binding['credential']['revision'], operation_id=binding['operation_id'],
            runtime_epoch=binding['epoch'], plan_hash=binding['plan_hash'], target=binding['target'],
            recipient=keys[0]['holder']['trust_domain'], recipient_key_id=keys[0]['id'], execution=binding['execution'])

    def resolve_execution(self, account, selector):
        if not isinstance(selector,dict) or not isinstance(selector.get('run_id'),str) or not selector['run_id']: raise CredentialDeliveryError()
        r=self._call('execution',dict(consumer_id=self.consumer,credential_key=account,session_id=selector.get('session_id') or '',run_id=selector['run_id']))
        if r.get('handling_mode')!=self.handling_mode or r.get('execution',{}).get('run_id')!=selector['run_id'] or selector.get('session_id') and r.get('execution',{}).get('session_id')!=selector['session_id'] or selector.get('agent_id') and r.get('execution',{}).get('agent_id')!=selector['agent_id']: raise CredentialDeliveryError()
        return r['execution']

    def current(self, binding):
        if self._call('check', self.request(binding)).get('current') is not True:
            raise CredentialDeliveryError('CREDENTIAL_DELIVERY_UNAVAILABLE', 503)

    def consume(self, binding, callback):
        fields = None
        try:
            context = self.request(binding)
            result = self._call('deliver', context)
            a, h, b, e = (result[k] for k in ('access', 'holder', 'binding', 'envelope'))
            if (e['type'] != 'sealed_for_workload' or h['boundary'] != 'workload' or e['recipient'] != context['recipient']
                or h['trust_domain'] != context['recipient'] or a['credential'] != binding['credential']
                or a['policy'] != {**({'handling_mode':self.handling_mode} if self.handling_mode=='application_managed' else {}),'allowed_plaintext_holders': [h], 'model_exposure': 'forbidden'}
                or a['target'] != {'purpose': {'type': 'extension'}, 'audience': f'application://{self.consumer}/{binding["account_ref"]}'}
                or a['usage']['consumer_id'] != self.consumer or a['usage']['material_type'] != self.material_type
                or not time.time()*1000 < e['expires_at_unix_ms'] <= time.time()*1000+121000):
                raise ValueError()
            prefix = 'awaken-recipient:v1:'
            ref = e['envelope_ref']['id']
            if not ref.startswith(prefix) or len(ref) > 100000: raise ValueError()
            payload = json.loads(base64.b64decode(ref[len(prefix):]))
            keys = json.loads(self.recipient_file.read_text())['keys']
            if len(keys) != 1 or keys[0]['id'] != payload['key_id'] or keys[0]['holder'] != h: raise ValueError()
            holder = dict(boundary=h['boundary'], trust_domain=h['trust_domain'])
            target = dict(purpose=dict(type='extension'), audience=a['target']['audience'])
            usage = dict(type='extension', consumer_id=self.consumer, material_type=self.material_type, public_config=a['usage']['public_config'])
            policy = {**({'handling_mode':self.handling_mode} if self.handling_mode=='application_managed' else {}),'allowed_plaintext_holders':[holder],'model_exposure':'forbidden'}
            aad = json.dumps([prefix, dict(id=a['credential']['id'], revision=a['credential']['revision']), a['material_source'],
                target, usage, policy, holder, dict(workspace_id=b['workspace_id'], target_use_fingerprint=b['target_use_fingerprint']),
                payload['key_id'], e['expires_at_unix_ms']], ensure_ascii=False, separators=(',', ':')).encode()
            plain = ChaCha20Poly1305(base64.b64decode(keys[0]['key_base64'])).decrypt(bytes(payload['nonce']), bytes(payload['ciphertext']), aad)
            opened = json.loads(plain)
            if opened['context'] != context or not opened['material'].startswith('awaken-credential-material-v1:'): raise ValueError()
            material = json.loads(opened['material'][len('awaken-credential-material-v1:'):])
            if material['type_id'] != self.material_type or len(material['fields'])>16 or any(not isinstance(v,str) for v in material['fields'].values()): raise ValueError()
            fields = material['fields']
            self.current(binding)
        except CredentialDeliveryError: raise
        except Exception:
            raise CredentialDeliveryError('CREDENTIAL_DELIVERY_UNAVAILABLE', 503) from None
        try:
            def use_field(name, write):
                if name not in fields or time.time()*1000 >= e['expires_at_unix_ms']:
                    raise CredentialDeliveryError('CREDENTIAL_DELIVERY_UNAVAILABLE', 503)
                self.current(binding)
                if time.time()*1000 >= e['expires_at_unix_ms']: raise CredentialDeliveryError()
                write(fields[name])
            callback(use_field)
        finally:
            if fields:
                for name in fields: fields[name] = ''
